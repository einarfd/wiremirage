use std::path::Path;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Result, Store};

use crate::bindings::Handler;
use crate::host_state::HostState;
use crate::store::{Bucket, Storage};

/// Wraps a wasmtime `Engine`, a `Linker` configured with all WireMirage
/// host imports, and the `Storage` backend that mints per-request buckets.
/// One `Runtime` is shared across requests; per-request state lives in
/// `HostState` instances created via `instantiate`.
pub struct Runtime {
    engine: Engine,
    linker: Linker<HostState>,
    storage: Storage,
}

impl Runtime {
    pub fn new(storage: Storage) -> Result<Self> {
        let engine = Engine::default();
        let mut linker: Linker<HostState> = Linker::new(&engine);
        Handler::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)?;
        Ok(Self {
            engine,
            linker,
            storage,
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
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
        let mut store = Store::new(&self.engine, HostState::new());
        let route = store.data_mut().push_bucket(route_bucket)?;
        let group = store.data_mut().push_bucket(group_bucket)?;
        let handler = Handler::instantiate(&mut store, component, &self.linker)?;
        Ok((handler, store, BucketHandles { route, group }))
    }
}

/// Resource handles returned alongside an instantiated `Handler` so the
/// caller can pass them as the `route-store` / `group-store` arguments to
/// `call_handle`.
pub struct BucketHandles {
    pub route: wasmtime::component::Resource<Bucket>,
    pub group: wasmtime::component::Resource<Bucket>,
}
