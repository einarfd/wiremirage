use wasmtime::Result;
use wasmtime::component::{Resource, ResourceTable};

use crate::bindings::wiremirage::handler::http::Host as HttpHost;
use crate::bindings::wiremirage::handler::log::{Host as LogHost, Level};
use crate::bindings::wiremirage::handler::store::{Host as StoreHost, HostBucket};
use crate::log::{LogCapture, LogLevel, LogRecord};
use crate::store::Bucket;

/// Per-invocation host state plumbed into the wasmtime `Store`.
///
/// One `HostState` is created per request. It owns the resource table for
/// the route-private and group-shared buckets opened from the shared
/// `Storage`, and accumulates handler logs via `log.emit`. Buckets are
/// thin views over the backing store; persistence lives in `Storage`.
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
    pub fn push_bucket(&mut self, bucket: Bucket) -> Result<Resource<Bucket>> {
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
        self.logs.push(LogRecord {
            level: level.into(),
            message,
        });
        Ok(())
    }
}
