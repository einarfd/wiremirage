use wasmtime::Result;
use wasmtime::component::{Resource, ResourceTable};

use crate::bindings::wiremirage::handler::http::Host as HttpHost;
use crate::bindings::wiremirage::handler::log::{Host as LogHost, Level};
use crate::bindings::wiremirage::handler::store::{Host as StoreHost, HostBucket};
use crate::log::{LogCapture, LogLevel, LogRecord};
use crate::store::MemBucket;

/// Per-invocation host state plumbed into the wasmtime `Store`.
///
/// One `HostState` is created per request, holds the route-private and
/// group-shared buckets in its resource table, and accumulates handler logs.
/// After `handle` returns, callers can inspect the captured logs and drop
/// the state — buckets only persist via the in-memory backend in slice 1
/// (Valkey-backed in slice 2).
pub struct HostState {
    table: ResourceTable,
    logs: LogCapture,
}

impl HostState {
    pub fn new() -> Self {
        Self {
            table: ResourceTable::new(),
            logs: LogCapture::new(),
        }
    }

    pub fn table_mut(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    /// Insert a bucket into the resource table and return its handle, ready
    /// to pass to a `borrow<bucket>` export parameter.
    pub fn push_bucket(&mut self, bucket: MemBucket) -> Result<Resource<MemBucket>> {
        Ok(self.table.push(bucket)?)
    }

    pub fn logs(&self) -> &[LogRecord] {
        self.logs.records()
    }

    pub fn take_logs(&mut self) -> Vec<LogRecord> {
        self.logs.take()
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

// -- http types-only interface ------------------------------------------------
// `http` defines no host functions, just record types. The Host trait is
// empty but must still be implemented for `add_to_linker` to type-check.

impl HttpHost for HostState {}

// -- store.bucket resource impl -----------------------------------------------

impl StoreHost for HostState {}

impl HostBucket for HostState {
    fn get(&mut self, self_: Resource<MemBucket>, key: String) -> Result<Option<Vec<u8>>> {
        let b = self.table.get(&self_)?;
        b.get(&key).map_err(Into::into)
    }

    fn set(&mut self, self_: Resource<MemBucket>, key: String, value: Vec<u8>) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set(key, value);
        Ok(())
    }

    fn delete(&mut self, self_: Resource<MemBucket>, key: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.delete(&key);
        Ok(())
    }

    fn incr(&mut self, self_: Resource<MemBucket>, key: String, by: i64) -> Result<i64> {
        let b = self.table.get_mut(&self_)?;
        b.incr(&key, by).map_err(Into::into)
    }

    fn list_keys(
        &mut self,
        self_: Resource<MemBucket>,
        prefix: Option<String>,
    ) -> Result<Vec<String>> {
        let b = self.table.get(&self_)?;
        Ok(b.list_keys(prefix.as_deref()))
    }

    fn list_push(&mut self, self_: Resource<MemBucket>, key: String, value: Vec<u8>) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.list_push(key, value).map_err(Into::into)
    }

    fn list_pop(&mut self, self_: Resource<MemBucket>, key: String) -> Result<Option<Vec<u8>>> {
        let b = self.table.get_mut(&self_)?;
        b.list_pop(&key).map_err(Into::into)
    }

    fn list_range(
        &mut self,
        self_: Resource<MemBucket>,
        key: String,
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>> {
        let b = self.table.get(&self_)?;
        b.list_range(&key, start, stop).map_err(Into::into)
    }

    fn list_length(&mut self, self_: Resource<MemBucket>, key: String) -> Result<u64> {
        let b = self.table.get(&self_)?;
        b.list_length(&key).map_err(Into::into)
    }

    fn hash_get(
        &mut self,
        self_: Resource<MemBucket>,
        key: String,
        field: String,
    ) -> Result<Option<Vec<u8>>> {
        let b = self.table.get(&self_)?;
        b.hash_get(&key, &field).map_err(Into::into)
    }

    fn hash_set(
        &mut self,
        self_: Resource<MemBucket>,
        key: String,
        field: String,
        value: Vec<u8>,
    ) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.hash_set(key, field, value).map_err(Into::into)
    }

    fn hash_delete(
        &mut self,
        self_: Resource<MemBucket>,
        key: String,
        field: String,
    ) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.hash_delete(&key, &field).map_err(Into::into)
    }

    fn hash_keys(&mut self, self_: Resource<MemBucket>, key: String) -> Result<Vec<String>> {
        let b = self.table.get(&self_)?;
        b.hash_keys(&key).map_err(Into::into)
    }

    fn set_add(&mut self, self_: Resource<MemBucket>, key: String, member: String) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set_add(key, member).map_err(Into::into)
    }

    fn set_remove(
        &mut self,
        self_: Resource<MemBucket>,
        key: String,
        member: String,
    ) -> Result<()> {
        let b = self.table.get_mut(&self_)?;
        b.set_remove(&key, &member).map_err(Into::into)
    }

    fn set_contains(
        &mut self,
        self_: Resource<MemBucket>,
        key: String,
        member: String,
    ) -> Result<bool> {
        let b = self.table.get(&self_)?;
        b.set_contains(&key, &member).map_err(Into::into)
    }

    fn drop(&mut self, rep: Resource<MemBucket>) -> Result<()> {
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
        self.logs.push(LogRecord {
            level: level.into(),
            message,
        });
        Ok(())
    }
}
