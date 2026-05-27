use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Result, Store};

use crate::bindings::Handler;
use crate::bindings::engine_bindings::Engine as EngineWorld;
use crate::host_state::{HandlerLimits, HostState};
use crate::store::{Bucket, Storage};

/// Per-call fuel budget (~1 instruction per unit). Calibrated for
/// componentize-js handlers, where SpiderMonkey embed startup alone
/// burns tens of millions of units before the handler runs. 10 B
/// fuel ≈ 1-2 s of pure CPU on aarch64 — generous for legitimate
/// mocks, strict enough that a `while(true)` traps with `out of
/// fuel` in bounded wall-clock time. Combined with the epoch
/// deadline below, whichever limit fires first wins.
pub const HANDLER_FUEL: u64 = 10_000_000_000;

/// Maximum linear-memory bytes per handler instance. componentize-js
/// components carry a full SpiderMonkey + steady-state ~16-32 MiB;
/// 64 MiB caps the abusable headroom without strangling legitimate
/// JSON-heavy handlers. wasm32 is 32-bit linear memory so the
/// architectural ceiling is 4 GiB regardless — this picks a
/// meaningful floor.
pub const HANDLER_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Wall-clock deadline expressed in epoch ticks. With
/// `EPOCH_TICK_INTERVAL_MS = 10`, 100 ticks ≈ 1s. The deadline is
/// a backstop for handlers that consume fuel slowly (heavy I/O via
/// host imports doesn't burn fuel) or use SIMD operations that have
/// a low fuel cost relative to their wall time.
pub const HANDLER_EPOCH_TICKS: u64 = 100;

/// Period between `Engine::increment_epoch()` calls from the
/// background ticker. 10 ms is the standard cadence for
/// epoch-interruption-based deadlines.
pub const EPOCH_TICK_INTERVAL_MS: u64 = 10;

/// Per-call resource budget for shared-engine (interpreted-language)
/// routes per ADR-0020's resource-limits section. The engine
/// instantiates and parses source inside the request budget, so the
/// numbers are an order of magnitude wider than the
/// per-route-component path's:
///
/// * `ENGINE_FUEL = u64::MAX` — fuel is effectively disabled. The
///   epoch deadline is the runaway-loop backstop. We can't turn
///   `consume_fuel` off per call (it's an engine-config flag), so
///   we set the per-call budget to the maximum and accept the
///   per-instruction accounting overhead.
/// * `ENGINE_EPOCH_TICKS = 3000` — ~30 s wall clock at the 10 ms
///   tick cadence. Long enough that "engine boots, parses 500 LoC,
///   runs the handler, returns" never trips it on a non-pathological
///   route; short enough that a `while(true)` dies in 30 s.
/// * `ENGINE_MAX_MEMORY_BYTES = 256 MiB` — comfortably fits a
///   SpiderMonkey instance plus a generous handler heap plus state.
pub const ENGINE_FUEL: u64 = u64::MAX;
pub const ENGINE_EPOCH_TICKS: u64 = 3000;
pub const ENGINE_MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// Wraps a wasmtime `Engine`, a `Linker` configured with all WireMirage
/// host imports, and the `Storage` backend that mints per-request buckets.
/// One `Runtime` is shared across requests; per-request state lives in
/// `HostState` instances created via `instantiate`.
pub struct Runtime {
    engine: Engine,
    linker: Linker<HostState>,
    storage: Storage,
    /// Per-call fuel budget applied in `instantiate_with_buckets`.
    /// Configurable via the test-only `with_handler_fuel` constructor;
    /// production code uses [`HANDLER_FUEL`].
    fuel: u64,
    /// Per-call epoch-tick deadline. Same story as `fuel` — defaults
    /// to [`HANDLER_EPOCH_TICKS`], overridable in tests.
    epoch_ticks: u64,
    /// Per-instance linear-memory cap. Defaults to
    /// [`HANDLER_MAX_MEMORY_BYTES`].
    max_memory_bytes: usize,
    /// Optional shared JS engine component + linker for the
    /// interpreted-language path (ADR-0020). `None` when no engine
    /// is wired in (tests that don't need it; pre-slice-57
    /// configurations). Loaded once via `with_js_engine`; the
    /// `Arc<Component>` is cheap to clone across requests.
    js_engine: Option<JsEngine>,
}

struct JsEngine {
    component: Arc<Component>,
    linker: Linker<HostState>,
}

/// Process-global memo of the precompiled engine artifact:
/// `(engine-bytes hash, serialized cwasm)`. See [`Runtime::engine_component`].
type EngineCwasmCache = Mutex<Option<(u64, Arc<Vec<u8>>)>>;

impl Runtime {
    pub fn new(storage: Storage) -> Result<Self> {
        Self::with_limits(
            storage,
            HANDLER_FUEL,
            HANDLER_EPOCH_TICKS,
            HANDLER_MAX_MEMORY_BYTES,
        )
    }

    /// Test-only constructor that lets the caller override the
    /// per-call limits. Production code uses [`Runtime::new`].
    /// Public because the host's integration-test crate is a
    /// separate target.
    pub fn with_limits(
        storage: Storage,
        fuel: u64,
        epoch_ticks: u64,
        max_memory_bytes: usize,
    ) -> Result<Self> {
        let mut config = Config::new();
        // Both flags must be on at engine-construction time —
        // toggling them on a live engine isn't supported. The matching
        // `Store::set_fuel` / `Store::set_epoch_deadline` calls in
        // `instantiate_with_buckets` make them effective per call.
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let mut linker: Linker<HostState> = Linker::new(&engine);
        Handler::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)?;
        Ok(Self {
            engine,
            linker,
            storage,
            fuel,
            epoch_ticks,
            max_memory_bytes,
            js_engine: None,
        })
    }

    /// Load the shared JS engine component from `path` and prepare
    /// a dedicated linker for it (with the `engine-host` import
    /// wired in addition to the usual store/log/http). Idempotent:
    /// a second call replaces the previous engine.
    ///
    /// Failure cases: file missing → returns Err; file isn't a
    /// component → returns Err; linker setup fails (rare, type
    /// mismatch with the wit) → returns Err. Each is a startup-
    /// time bug from the operator's POV.
    pub fn with_js_engine(mut self, path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let component = self.engine_component(&bytes)?;
        self.attach_engine(component);
        Ok(self)
    }

    /// Same as `with_js_engine` but takes the component bytes
    /// directly. The host binary `include_bytes!`-embeds the
    /// vendored `js-engine.wasm` and feeds it through here so the
    /// runtime has no on-disk filesystem dependency.
    pub fn with_js_engine_bytes(mut self, bytes: &[u8]) -> Result<Self> {
        let component = self.engine_component(bytes)?;
        self.attach_engine(component);
        Ok(self)
    }

    /// Build the engine `Component` while Cranelift-compiling the ~12 MB
    /// StarlingMonkey wasm **at most once per machine**, not once per
    /// `Runtime`. The compile is the dominant cost on the engine path
    /// (~30 s in debug); every fresh `Runtime` (each test makes one)
    /// would otherwise recompile from scratch — that's what made the
    /// engine-backed test suite slow.
    ///
    /// Memoized at two levels, both keyed by a hash of the engine bytes
    /// (so a changed engine recompiles rather than loading stale code).
    /// In-process (`ENGINE_CWASM`): the first `Runtime` precompiles to a
    /// serialized artifact and the rest reuse those bytes. Cross-process:
    /// the first process to need the engine writes a `.cwasm` to the temp
    /// dir, and every other process — the next test binary, a restarted
    /// host — reads + `deserialize`s it, skipping the compile entirely.
    /// The read is a plain file load (lock-free), avoiding the per-entry
    /// lock contention that made wasmtime's built-in on-disk cache
    /// *slower* under parallel tests.
    ///
    /// Every `Runtime` builds its `Engine` from the same `Config`
    /// (`with_limits`), so an artifact precompiled by one loads in any
    /// other. A stale on-disk artifact (e.g. after a wasmtime upgrade)
    /// fails `deserialize` and is transparently recompiled + rewritten,
    /// so the cache is self-healing and needs no manual invalidation.
    fn engine_component(&self, bytes: &[u8]) -> Result<Component> {
        static ENGINE_CWASM: OnceLock<EngineCwasmCache> = OnceLock::new();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        };
        let slot = ENGINE_CWASM.get_or_init(|| Mutex::new(None));
        let mut guard = slot.lock().expect("poisoned");
        if let Some((h, cwasm)) = guard.as_ref()
            && *h == hash
        {
            // In-process hit: deserialize the memoized bytes for this
            // `Runtime`'s engine. (Each `Runtime` has its own `Engine`,
            // so the Component can't be shared — only the bytes.)
            let cwasm = cwasm.clone();
            drop(guard);
            // SAFETY: produced by `precompile_component` on an `Engine`
            // with the identical `Config` + wasmtime version.
            return unsafe { Component::deserialize(&self.engine, cwasm.as_slice()) };
        }
        let (component, cwasm) = self.load_or_precompile_engine(bytes, hash)?;
        *guard = Some((hash, cwasm));
        Ok(component)
    }

    /// Cross-process layer behind [`Self::engine_component`]: load the
    /// precompiled artifact from the temp-dir cache, or compile it and
    /// write it there for the next process. Returns the `Component` for
    /// *this* `Runtime` plus the serialized bytes to memoize for sibling
    /// `Runtime`s in the same process.
    fn load_or_precompile_engine(
        &self,
        bytes: &[u8],
        hash: u64,
    ) -> Result<(Component, Arc<Vec<u8>>)> {
        let cache_path =
            std::env::temp_dir().join(format!("wiremirage-engine-v1-{hash:016x}.cwasm"));

        // Cache hit: read + deserialize. A read miss, or a stale artifact
        // that fails to deserialize (wasmtime upgrade, config drift),
        // simply falls through to a recompile below.
        if let Ok(cached) = std::fs::read(&cache_path) {
            // SAFETY: see `engine_component`. An incompatible artifact
            // returns Err here rather than misbehaving, and we recompile.
            if let Ok(component) = unsafe { Component::deserialize(&self.engine, &cached) } {
                return Ok((component, Arc::new(cached)));
            }
        }

        let compiled = self.engine.precompile_component(bytes)?;
        // SAFETY: just produced by this engine's `precompile_component`.
        let component = unsafe { Component::deserialize(&self.engine, &compiled)? };
        // Publish for other processes. Write to a unique temp path then
        // rename so a concurrent reader never sees a half-written file;
        // best-effort — a failed cache write just means the next process
        // recompiles.
        let tmp = cache_path.with_extension(format!("tmp.{}", std::process::id()));
        if std::fs::write(&tmp, &compiled).is_ok() {
            let _ = std::fs::rename(&tmp, &cache_path);
        }
        Ok((component, Arc::new(compiled)))
    }

    fn attach_engine(&mut self, component: Component) {
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        EngineWorld::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)
            .expect("engine linker setup");
        self.js_engine = Some(JsEngine {
            component: Arc::new(component),
            linker,
        });
    }

    /// True when `with_js_engine` has been called successfully and
    /// shared-engine dispatch is available.
    pub fn has_js_engine(&self) -> bool {
        self.js_engine.is_some()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn handler_fuel(&self) -> u64 {
        self.fuel
    }

    /// Spawn a background tokio task that advances the engine's
    /// epoch on a fixed cadence. Required when
    /// `epoch_interruption(true)` is set on the engine config —
    /// without it, `Store::set_epoch_deadline` is configured but
    /// the deadline never fires. Returns the spawn handle; production
    /// code drops it (the task lives for the engine's lifetime).
    /// Tests can hold it if they want to verify shutdown behavior.
    pub fn spawn_epoch_ticker(&self) -> tokio::task::JoinHandle<()> {
        let engine = self.engine.clone();
        let interval = Duration::from_millis(EPOCH_TICK_INTERVAL_MS);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                engine.increment_epoch();
            }
        })
    }

    /// Compile a component from a file (typically a `.component.wasm`
    /// produced by `wasm-tools component new`).
    pub fn load_component(&self, path: &Path) -> Result<Component> {
        Component::from_file(&self.engine, path)
    }

    /// Instantiate the component with fresh buckets for the given
    /// `(group, route)` scope. Returns the `Handler`, the wasmtime `Store`
    /// (which owns the state and resource handles), and the bucket
    /// resources to pass to `call_handle`.
    pub fn instantiate(
        &self,
        component: &Component,
        group_ulid: &str,
        route_ulid: &str,
    ) -> Result<(Handler, Store<HostState>, BucketHandles)> {
        let route_bucket = self
            .storage
            .route_bucket(group_ulid, route_ulid)
            .map_err(|e| wasmtime::Error::msg(format!("open route bucket: {e}")))?;
        let group_bucket = self
            .storage
            .group_bucket(group_ulid)
            .map_err(|e| wasmtime::Error::msg(format!("open group bucket: {e}")))?;
        self.instantiate_with_buckets(component, route_bucket, group_bucket)
    }

    /// Lower-level entry point used by tests that want to construct buckets
    /// explicitly (e.g., to seed state before instantiation).
    pub fn instantiate_with_buckets(
        &self,
        component: &Component,
        route_bucket: Bucket,
        group_bucket: Bucket,
    ) -> Result<(Handler, Store<HostState>, BucketHandles)> {
        let mut store = Store::new(
            &self.engine,
            HostState::new(HandlerLimits::new(self.max_memory_bytes)),
        );
        // Fuel + epoch deadline + memory limiter must all be set
        // before the handler runs. Skipping any of them silently
        // drops back to "no limit" behavior — that's why these live
        // here rather than at the call site.
        store.set_fuel(self.fuel)?;
        store.set_epoch_deadline(self.epoch_ticks);
        store.limiter(|state| &mut state.limits);

        let route = store.data_mut().push_bucket(route_bucket)?;
        let group = store.data_mut().push_bucket(group_bucket)?;
        let handler = Handler::instantiate(&mut store, component, &self.linker)?;
        Ok((handler, store, BucketHandles { route, group }))
    }

    /// Instantiate the shared JS engine for `(group, route)` with
    /// `source` as the per-request handler bytes. Returns the
    /// engine-world handle (with the same `call_handle` shape as
    /// `Handler`), the wasmtime store, and the bucket resources.
    /// Errors out if `with_js_engine` hasn't been called yet.
    pub fn instantiate_engine(
        &self,
        group_ulid: &str,
        route_ulid: &str,
        source: String,
    ) -> Result<(EngineWorld, Store<HostState>, BucketHandles)> {
        let route_bucket = self
            .storage
            .route_bucket(group_ulid, route_ulid)
            .map_err(|e| wasmtime::Error::msg(format!("open route bucket: {e}")))?;
        let group_bucket = self
            .storage
            .group_bucket(group_ulid)
            .map_err(|e| wasmtime::Error::msg(format!("open group bucket: {e}")))?;
        self.instantiate_engine_with_buckets(source, route_bucket, group_bucket)
    }

    pub fn instantiate_engine_with_buckets(
        &self,
        source: String,
        route_bucket: Bucket,
        group_bucket: Bucket,
    ) -> Result<(EngineWorld, Store<HostState>, BucketHandles)> {
        let Some(engine) = &self.js_engine else {
            return Err(wasmtime::Error::msg(
                "shared JS engine not configured (call Runtime::with_js_engine on startup)",
            ));
        };
        let mut state = HostState::new(HandlerLimits::new(ENGINE_MAX_MEMORY_BYTES));
        state.set_current_source(source);
        let mut store = Store::new(&self.engine, state);
        // Wider per-call budget for the interpreted path. Fuel is
        // effectively disabled (set to max); epoch is the
        // runaway-loop backstop.
        store.set_fuel(ENGINE_FUEL)?;
        store.set_epoch_deadline(ENGINE_EPOCH_TICKS);
        store.limiter(|state| &mut state.limits);

        let route = store.data_mut().push_bucket(route_bucket)?;
        let group = store.data_mut().push_bucket(group_bucket)?;
        let engine_world = EngineWorld::instantiate(&mut store, &engine.component, &engine.linker)?;
        Ok((engine_world, store, BucketHandles { route, group }))
    }
}

/// Resource handles returned alongside an instantiated `Handler` so the
/// caller can pass them as the `route-store` / `group-store` arguments to
/// `call_handle`.
pub struct BucketHandles {
    pub route: wasmtime::component::Resource<Bucket>,
    pub group: wasmtime::component::Resource<Bucket>,
}
