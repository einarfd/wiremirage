use wasmtime::Result;
use wasmtime::component::{Resource, ResourceTable};

use crate::bindings::wiremirage::handler::http::Host as HttpHost;
use crate::bindings::wiremirage::handler::log::{Host as LogHost, Level};
use crate::bindings::wiremirage::handler::store::{Host as StoreHost, HostBucket};
use crate::log::{LogCapture, LogLevel, LogRecord};
use crate::store::Bucket;

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
}

impl HostState {
    pub fn new(limits: HandlerLimits) -> Self {
        Self {
            table: ResourceTable::new(),
            logs: LogCapture::new(),
            limits,
            current_source: None,
        }
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

// -- engine-world bindings (ADR-0020) -----------------------------------------
//
// `wasmtime::component::bindgen!` generates one set of Host traits
// per world. The handler world and the engine world share the same
// `wiremirage:handler` package, but the generated trait types are
// nominally distinct — so `HostState` has to impl each set
// separately. The bodies are identical to the handler-world impls
// above; bucket + log methods delegate to the same fields. The
// engine-world adds `engine-host.get-source` on top.

use crate::bindings::engine_bindings::wiremirage::handler::engine_host::Host as EngineHostHost;
use crate::bindings::engine_bindings::wiremirage::handler::http::Host as EngineHttpHost;
use crate::bindings::engine_bindings::wiremirage::handler::log::{
    Host as EngineLogHost, Level as EngineLogLevel,
};
use crate::bindings::engine_bindings::wiremirage::handler::store::{
    Host as EngineStoreHost, HostBucket as EngineHostBucket,
};

impl EngineHttpHost for HostState {}
impl EngineStoreHost for HostState {}

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
