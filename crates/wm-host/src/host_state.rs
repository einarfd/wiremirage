use std::sync::OnceLock;
use std::time::{Duration, Instant};

use wasmtime::Result;
use wasmtime::component::{Resource, ResourceTable};

use crate::bindings::wiremirage::handler::clock::Host as ClockHost;
use crate::bindings::wiremirage::handler::http::Host as HttpHost;
use crate::bindings::wiremirage::handler::log::{Host as LogHost, Level};
use crate::bindings::wiremirage::handler::store::{Host as StoreHost, HostBucket};
use crate::log::{LogCapture, LogLevel, LogRecord};
use crate::store::Bucket;

/// Anchor for the `clock.monotonic-ms` host import (ADR-0021).
/// Initialised lazily on first use and never reset, so the value the
/// host returns is "milliseconds since this wm-host process first
/// looked at the clock" — opaque, monotonically non-decreasing, only
/// meaningful as a difference.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Upper bound on a single `clock.sleep` call. The wasm epoch
/// deadline (slice 46: 1 s for AOT handlers, 30 s for the shared
/// engine — ADR-0020) is what would otherwise stop a runaway sleep,
/// but epoch interruption only fires when wasm bytecode runs; inside
/// the host import the wasm is paused and can't be trapped. The
/// clamp keeps a single tokio worker from being blocked longer than
/// the engine's outer deadline would ever allow.
const MAX_SLEEP_MS: u64 = 30_000;

/// Wasmtime `ResourceLimiter` impl that caps linear-memory growth and
/// records the peak byte count for the journal entry. Lives inside
/// `HostState` so `Store::limiter` can reach it via the closure
/// passed to `instantiate_with_buckets`.
#[derive(Debug, Clone, Copy)]
pub struct HandlerLimits {
    /// Hard ceiling on per-instance linear-memory bytes. A
    /// `memory_growing` request above this denies the grow, which
    /// wasmtime surfaces as a trap.
    pub max_memory_bytes: usize,
    /// High-water mark of bytes the handler actually used. Updated
    /// every time `memory_growing` is allowed; reported via the
    /// journal entry's `resources.memory_peak_bytes` field.
    pub peak_memory_bytes: usize,
}

impl HandlerLimits {
    pub fn new(max_memory_bytes: usize) -> Self {
        Self {
            max_memory_bytes,
            peak_memory_bytes: 0,
        }
    }
}

impl wasmtime::ResourceLimiter for HandlerLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        if desired > self.max_memory_bytes {
            return Ok(false);
        }
        if desired > self.peak_memory_bytes {
            self.peak_memory_bytes = desired;
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        // Function tables are tiny and bounded by component-model
        // semantics — no cap here. Memory is the only realistic abuse
        // vector for SpiderMonkey-based handlers.
        Ok(true)
    }
}

/// The status + headers a streaming handler commits via
/// `response-stream.start` (ADR-0022). Sent to the dispatch path over a
/// oneshot so it can build the axum response head and begin streaming
/// the body while the handler keeps running.
#[derive(Debug, Clone)]
pub struct StreamHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Sink wiring a streaming handler to the dispatch path. The handler
/// (running on a `spawn_blocking` thread) pushes the head over a
/// oneshot and each body chunk over a bounded mpsc; the dispatch task
/// holds the receivers, returns the axum response as soon as the head
/// arrives, and streams the chunks to the wire. Bounded so a slow
/// client backpressures the handler (blocking_send parks the wasm
/// thread); a dropped receiver (client gone) makes `write-chunk`
/// return false.
struct StreamSink {
    head: Option<tokio::sync::oneshot::Sender<StreamHead>>,
    chunks: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    started: bool,
    /// When `start` was called — anchors the streaming-budget check in
    /// the engine's epoch-deadline callback (ADR-0022 slice 2).
    started_at: Option<std::time::Instant>,
    /// Running totals for the journal entry (ADR-0022 slice 2).
    chunk_count: u64,
    byte_count: u64,
    /// Set when a `write-chunk` failed because the client had gone.
    client_disconnected: bool,
}

/// Per-invocation host state plumbed into the wasmtime `Store`.
///
/// One `HostState` is created per request. It owns the resource table for
/// the route-private and group-shared buckets opened from the shared
/// `Storage`, and accumulates handler logs via `log.emit`. Buckets are
/// thin views over the backing store; persistence lives in `Storage`.
pub struct HostState {
    table: ResourceTable,
    logs: LogCapture,
    /// Memory cap + peak tracker. `Store::limiter` returns a `&mut`
    /// to this field every time wasmtime checks a grow request.
    pub limits: HandlerLimits,
    /// JS source the shared engine asks for on every `handle` call.
    /// `None` for the user-facing `handler` world (per-route
    /// components); `Some(...)` for the `engine` world (shared
    /// engine + per-route source). Set once before the engine
    /// instantiates by `Runtime::instantiate_engine` (slice 57 /
    /// ADR-0020).
    current_source: Option<String>,
    /// Streaming-response sink (ADR-0022). `None` unless the dispatch
    /// path wired it before the engine call; only the engine world
    /// imports `response-stream`, so per-route components never set it.
    stream: Option<StreamSink>,
    /// Whether this request's handler is allowed to schedule outbound
    /// callbacks (ADR-0034). The dispatch path sets this to `host egress
    /// enabled && group.callout_enabled`. When false, `callback.schedule`
    /// is rejected synchronously with a clear error.
    callouts_allowed: bool,
    /// Callbacks the handler scheduled via `callback.schedule`, drained by
    /// the dispatch path after `handle` returns and fired on background
    /// tasks (ADR-0034). Empty for the handler world (no callback import).
    scheduled_callbacks: Vec<crate::callout::ScheduledCallback>,
}

/// Max callbacks a single handler invocation may schedule — a bound on
/// fan-out abuse / a runaway loop scheduling callbacks (ADR-0034).
const MAX_CALLBACKS_PER_REQUEST: usize = 16;

impl HostState {
    pub fn new(limits: HandlerLimits) -> Self {
        Self {
            table: ResourceTable::new(),
            logs: LogCapture::new(),
            limits,
            current_source: None,
            stream: None,
            callouts_allowed: false,
            scheduled_callbacks: Vec::new(),
        }
    }

    /// Wire this invocation for streaming responses. The dispatch path
    /// holds the matching receivers; when the handler calls
    /// `response-stream.start` the head goes out the oneshot and the
    /// dispatch begins streaming chunks from the mpsc.
    pub fn set_response_stream_sink(
        &mut self,
        head: tokio::sync::oneshot::Sender<StreamHead>,
        chunks: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) {
        self.stream = Some(StreamSink {
            head: Some(head),
            chunks: Some(chunks),
            started: false,
            started_at: None,
            chunk_count: 0,
            byte_count: 0,
            client_disconnected: false,
        });
    }

    /// Whether the handler switched this response to streaming mode
    /// (called `response-stream.start`). The dispatch path uses the
    /// streamed body in that case and ignores `handle`'s return value.
    pub fn streaming_started(&self) -> bool {
        self.stream.as_ref().is_some_and(|s| s.started)
    }

    /// Wall-clock since `response-stream.start`, or `None` if the
    /// handler hasn't started streaming. Drives the streaming-budget
    /// check in the engine epoch-deadline callback.
    pub fn streaming_elapsed(&self) -> Option<std::time::Duration> {
        self.stream
            .as_ref()
            .and_then(|s| s.started_at)
            .map(|t| t.elapsed())
    }

    /// Final streaming stats for the journal entry: `(chunks, bytes,
    /// client_disconnected)`. `None` if the handler didn't stream.
    pub fn stream_stats(&self) -> Option<(u64, u64, bool)> {
        self.stream
            .as_ref()
            .filter(|s| s.started)
            .map(|s| (s.chunk_count, s.byte_count, s.client_disconnected))
    }

    pub fn table_mut(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    /// Insert a bucket into the resource table and return its handle, ready
    /// to pass to a `borrow<bucket>` export parameter.
    pub fn push_bucket(&mut self, bucket: Bucket) -> Result<Resource<Bucket>> {
        Ok(self.table.push(bucket)?)
    }

    pub fn logs(&self) -> &[LogRecord] {
        self.logs.records()
    }

    pub fn take_logs(&mut self) -> Vec<LogRecord> {
        self.logs.take()
    }

    /// Set the source the engine's `get-source` import will return
    /// for this request. Called by `Runtime::instantiate_engine`
    /// before the engine runs.
    pub fn set_current_source(&mut self, source: String) {
        self.current_source = Some(source);
    }

    /// Allow (or forbid) this invocation's handler from scheduling outbound
    /// callbacks (ADR-0034). Set by the dispatch path from `host egress
    /// enabled && group.callout_enabled` before the engine runs.
    pub fn set_callouts_allowed(&mut self, allowed: bool) {
        self.callouts_allowed = allowed;
    }

    /// Drain the callbacks the handler scheduled this request, for the
    /// dispatch path to fire on background tasks. Leaves the buffer empty.
    pub fn take_scheduled_callbacks(&mut self) -> Vec<crate::callout::ScheduledCallback> {
        std::mem::take(&mut self.scheduled_callbacks)
    }
}

impl Default for HostState {
    fn default() -> Self {
        // A permissive default for tests that don't care about memory
        // limits. Production code goes through `Runtime`, which
        // installs the configured cap instead.
        Self::new(HandlerLimits::new(usize::MAX))
    }
}

// -- http types-only interface ------------------------------------------------
// `http` defines no host functions, just record types. The Host trait is
// empty but must still be implemented for `add_to_linker` to type-check.

impl HttpHost for HostState {}

// -- store.bucket resource impl -----------------------------------------------
//
// Bucket ops all take `&mut self` because the Valkey variant needs `&mut
// redis::Connection`; even read-only ops therefore use `table.get_mut`.

impl StoreHost for HostState {}

impl HostBucket for HostState {
    fn get(&mut self, self_: Resource<Bucket>, key: String) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.get(&key).map_err(Into::into)
    }

    fn set(&mut self, self_: Resource<Bucket>, key: String, value: Vec<u8>) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set(&key, value).map_err(Into::into)
    }

    fn delete(&mut self, self_: Resource<Bucket>, key: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.delete(&key).map_err(Into::into)
    }

    fn incr(&mut self, self_: Resource<Bucket>, key: String, by: i64) -> Result<i64> {
        let b = self.table.get_mut(&self_)?;
        b.incr(&key, by).map_err(Into::into)
    }

    fn list_keys(
        &mut self,
        self_: Resource<Bucket>,
        prefix: Option<String>,
    ) -> Result<Vec<String>> {
        let b = self.table.get_mut(&self_)?;
        b.list_keys(prefix.as_deref()).map_err(Into::into)
    }

    fn list_push(&mut self, self_: Resource<Bucket>, key: String, value: Vec<u8>) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.list_push(&key, value).map_err(Into::into)
    }

    fn list_pop(&mut self, self_: Resource<Bucket>, key: String) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.list_pop(&key).map_err(Into::into)
    }

    fn list_range(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.list_range(&key, start, stop).map_err(Into::into)
    }

    fn list_length(&mut self, self_: Resource<Bucket>, key: String) -> Result<u64> {
        let b = self.table.get_mut(&self_)?;
        b.list_length(&key).map_err(Into::into)
    }

    fn hash_get(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        field: String,
    ) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.hash_get(&key, &field).map_err(Into::into)
    }

    fn hash_set(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        field: String,
        value: Vec<u8>,
    ) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.hash_set(&key, &field, value).map_err(Into::into)
    }

    fn hash_delete(&mut self, self_: Resource<Bucket>, key: String, field: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.hash_delete(&key, &field).map_err(Into::into)
    }

    fn hash_keys(&mut self, self_: Resource<Bucket>, key: String) -> Result<Vec<String>> {
        let b = self.table.get_mut(&self_)?;
        b.hash_keys(&key).map_err(Into::into)
    }

    fn set_add(&mut self, self_: Resource<Bucket>, key: String, member: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set_add(&key, &member).map_err(Into::into)
    }

    fn set_remove(&mut self, self_: Resource<Bucket>, key: String, member: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set_remove(&key, &member).map_err(Into::into)
    }

    fn set_contains(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        member: String,
    ) -> Result<bool> {
        let b = self.table.get_mut(&self_)?;
        b.set_contains(&key, &member).map_err(Into::into)
    }

    fn drop(&mut self, rep: Resource<Bucket>) -> Result<()> {
        let _ = self.table.delete(rep)?;
        Ok(())
    }
}

// -- log impl -----------------------------------------------------------------

impl From<Level> for LogLevel {
    fn from(l: Level) -> Self {
        match l {
            Level::Debug => LogLevel::Debug,
            Level::Info => LogLevel::Info,
            Level::Warn => LogLevel::Warn,
            Level::Error => LogLevel::Error,
        }
    }
}

impl LogHost for HostState {
    fn emit(&mut self, level: Level, message: String) -> Result<()> {
        self.logs.push_now(level.into(), message);
        Ok(())
    }
}

// -- clock impl (ADR-0021) ----------------------------------------------------
//
// Three host imports give handlers latency simulation (`sleep`) and the
// usual wall-vs-monotonic time split. Sleep semantics worth knowing:
//
// 1. The host blocks the calling thread for the requested duration.
//    On the multi-thread tokio runtime that production uses, we wrap
//    with `block_in_place` so the worker is freed for other tasks
//    while this one parks. On the current-thread runtime (tests),
//    `block_in_place` would panic, so we fall back to plain
//    `thread::sleep` — which blocks the test's single worker but
//    that's fine since tests don't run concurrent requests.
//
// 2. The sleep duration counts against the wasm epoch deadline.
//    Inside the host import the wasm is paused and the epoch
//    interrupter can't trap it, so a clamp (`MAX_SLEEP_MS`) is what
//    actually bounds the per-call sleep. After sleep returns, the
//    next wasm instruction will hit the epoch check; a handler that
//    sleeps close to the deadline has little room left for its own
//    code. JS/TS handlers (shared-engine path) get the 30 s outer
//    deadline; AOT components get 1 s. Document this in the handler
//    docs so operators know the budget they're working against.

fn do_sleep(ms: u64) {
    let dur = Duration::from_millis(ms.min(MAX_SLEEP_MS));
    if dur.is_zero() {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current()
        && matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        )
    {
        // Cooperate with the multi-thread runtime: hand the worker
        // back so it can serve other tasks while this one parks.
        tokio::task::block_in_place(|| std::thread::sleep(dur));
    } else {
        // Current-thread runtime (tests) — block_in_place would
        // panic, so just sleep on the current OS thread.
        std::thread::sleep(dur);
    }
}

fn monotonic_now_ms() -> u64 {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u64
}

fn wall_now_ms() -> u64 {
    // `chrono::Utc::now().timestamp_millis()` is signed; in practice
    // it's far from 0 or i64::MAX, but the cast handles the edge by
    // clamping pre-1970 values to 0.
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

impl ClockHost for HostState {
    fn sleep(&mut self, ms: u64) -> Result<()> {
        do_sleep(ms);
        Ok(())
    }

    fn wall_time_ms(&mut self) -> Result<u64> {
        Ok(wall_now_ms())
    }

    fn monotonic_ms(&mut self) -> Result<u64> {
        Ok(monotonic_now_ms())
    }
}

// -- engine-world bindings (ADR-0020) -----------------------------------------
//
// `wasmtime::component::bindgen!` generates one set of Host traits
// per world. The handler world and the engine world share the same
// `wiremirage:handler` package, but the generated trait types are
// nominally distinct — so `HostState` has to impl each set
// separately. The bodies are identical to the handler-world impls
// above; bucket + log methods delegate to the same fields. The
// engine-world adds `engine-host.get-source` on top.

use crate::bindings::engine_bindings::wiremirage::handler::callback::Host as EngineCallbackHost;
use crate::bindings::engine_bindings::wiremirage::handler::clock::Host as EngineClockHost;
use crate::bindings::engine_bindings::wiremirage::handler::engine_host::Host as EngineHostHost;
use crate::bindings::engine_bindings::wiremirage::handler::http::Host as EngineHttpHost;
use crate::bindings::engine_bindings::wiremirage::handler::log::{
    Host as EngineLogHost, Level as EngineLogLevel,
};
use crate::bindings::engine_bindings::wiremirage::handler::response_stream::Host as EngineResponseStreamHost;
use crate::bindings::engine_bindings::wiremirage::handler::store::{
    Host as EngineStoreHost, HostBucket as EngineHostBucket,
};

impl EngineHttpHost for HostState {}
impl EngineStoreHost for HostState {}

impl EngineResponseStreamHost for HostState {
    fn start(&mut self, status: u16, headers: Vec<(String, String)>) -> Result<()> {
        // First `start` wins. Send the head over the oneshot so the
        // dispatch task can build the response head and begin
        // streaming. If the sink isn't wired (e.g. dry-run) or `start`
        // was already called, this is a no-op.
        if let Some(sink) = self.stream.as_mut()
            && let Some(head) = sink.head.take()
        {
            sink.started = true;
            sink.started_at = Some(std::time::Instant::now());
            // Receiver dropped (client already gone) → nothing to do.
            let _ = head.send(StreamHead { status, headers });
        }
        Ok(())
    }

    fn write_chunk(&mut self, bytes: Vec<u8>) -> Result<bool> {
        // Require `start` first: until the head is sent the dispatch
        // task hasn't begun draining the chunk channel, so a blocking
        // send here could fill the buffer and deadlock. The shim's
        // `host.responseStream` always calls `start` before handing
        // back the writer, so this only guards misuse of the raw imports.
        let Some(sink) = self.stream.as_mut() else {
            return Ok(false);
        };
        if !sink.started {
            return Ok(false);
        }
        let Some(tx) = sink.chunks.as_ref() else {
            return Ok(false);
        };
        let len = bytes.len() as u64;
        // `blocking_send` parks this (spawn_blocking) thread when the
        // channel is full — that's the backpressure. An error means the
        // receiver was dropped: the client disconnected, so report
        // false and let the handler stop early.
        match tx.blocking_send(bytes) {
            Ok(()) => {
                sink.chunk_count += 1;
                sink.byte_count += len;
                Ok(true)
            }
            Err(_) => {
                sink.client_disconnected = true;
                Ok(false)
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        // Drop the sender so the body stream sees end-of-stream.
        if let Some(sink) = self.stream.as_mut() {
            sink.chunks = None;
        }
        Ok(())
    }
}

impl EngineClockHost for HostState {
    fn sleep(&mut self, ms: u64) -> Result<()> {
        do_sleep(ms);
        Ok(())
    }
    fn wall_time_ms(&mut self) -> Result<u64> {
        Ok(wall_now_ms())
    }
    fn monotonic_ms(&mut self) -> Result<u64> {
        Ok(monotonic_now_ms())
    }
}

impl EngineLogHost for HostState {
    fn emit(&mut self, level: EngineLogLevel, message: String) -> Result<()> {
        let mapped = match level {
            EngineLogLevel::Debug => LogLevel::Debug,
            EngineLogLevel::Info => LogLevel::Info,
            EngineLogLevel::Warn => LogLevel::Warn,
            EngineLogLevel::Error => LogLevel::Error,
        };
        self.logs.push_now(mapped, message);
        Ok(())
    }
}

impl EngineCallbackHost for HostState {
    fn schedule(
        &mut self,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        delay_ms: u64,
    ) -> Result<std::result::Result<(), String>> {
        // Synchronous rejections the handler can catch: the capability gates
        // (host egress off / group not opted in) and basic shape. The per-IP
        // egress decision is made at fire time and journaled — it can't be
        // known here without resolving, and we don't block the handler on DNS.
        if !self.callouts_allowed {
            return Ok(Err(
                "outbound callbacks are not enabled: the host must run with WM_EGRESS on and \
                 this group must set callout_enabled"
                    .to_string(),
            ));
        }
        if url.trim().is_empty() {
            return Ok(Err("callback url must not be empty".to_string()));
        }
        if self.scheduled_callbacks.len() >= MAX_CALLBACKS_PER_REQUEST {
            return Ok(Err(format!(
                "too many callbacks scheduled in one request (max {MAX_CALLBACKS_PER_REQUEST})"
            )));
        }
        self.scheduled_callbacks
            .push(crate::callout::ScheduledCallback {
                url,
                method,
                headers,
                body,
                delay_ms,
            });
        Ok(Ok(()))
    }
}

impl EngineHostHost for HostState {
    fn get_source(&mut self) -> Result<String> {
        // `current_source` is set by `Runtime::instantiate_engine`
        // before this is reachable; a panic here would mean a
        // dispatch-path bug, not user error.
        Ok(self
            .current_source
            .clone()
            .unwrap_or_else(|| String::from("// engine source unset (host bug)\n")))
    }
}

impl EngineHostBucket for HostState {
    fn get(&mut self, self_: Resource<Bucket>, key: String) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.get(&key).map_err(Into::into)
    }
    fn set(&mut self, self_: Resource<Bucket>, key: String, value: Vec<u8>) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set(&key, value).map_err(Into::into)
    }
    fn delete(&mut self, self_: Resource<Bucket>, key: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.delete(&key).map_err(Into::into)
    }
    fn incr(&mut self, self_: Resource<Bucket>, key: String, by: i64) -> Result<i64> {
        let b = self.table.get_mut(&self_)?;
        b.incr(&key, by).map_err(Into::into)
    }
    fn list_keys(
        &mut self,
        self_: Resource<Bucket>,
        prefix: Option<String>,
    ) -> Result<Vec<String>> {
        let b = self.table.get_mut(&self_)?;
        b.list_keys(prefix.as_deref()).map_err(Into::into)
    }
    fn list_push(&mut self, self_: Resource<Bucket>, key: String, value: Vec<u8>) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.list_push(&key, value).map_err(Into::into)
    }
    fn list_pop(&mut self, self_: Resource<Bucket>, key: String) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.list_pop(&key).map_err(Into::into)
    }
    fn list_range(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.list_range(&key, start, stop).map_err(Into::into)
    }
    fn list_length(&mut self, self_: Resource<Bucket>, key: String) -> Result<u64> {
        let b = self.table.get_mut(&self_)?;
        b.list_length(&key).map_err(Into::into)
    }
    fn hash_get(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        field: String,
    ) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.hash_get(&key, &field).map_err(Into::into)
    }
    fn hash_set(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        field: String,
        value: Vec<u8>,
    ) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.hash_set(&key, &field, value).map_err(Into::into)
    }
    fn hash_delete(&mut self, self_: Resource<Bucket>, key: String, field: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.hash_delete(&key, &field).map_err(Into::into)
    }
    fn hash_keys(&mut self, self_: Resource<Bucket>, key: String) -> Result<Vec<String>> {
        let b = self.table.get_mut(&self_)?;
        b.hash_keys(&key).map_err(Into::into)
    }
    fn set_add(&mut self, self_: Resource<Bucket>, key: String, member: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set_add(&key, &member).map_err(Into::into)
    }
    fn set_remove(&mut self, self_: Resource<Bucket>, key: String, member: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set_remove(&key, &member).map_err(Into::into)
    }
    fn set_contains(
        &mut self,
        self_: Resource<Bucket>,
        key: String,
        member: String,
    ) -> Result<bool> {
        let b = self.table.get_mut(&self_)?;
        b.set_contains(&key, &member).map_err(Into::into)
    }
    fn drop(&mut self, rep: Resource<Bucket>) -> Result<()> {
        let _ = self.table.delete(rep)?;
        Ok(())
    }
}
