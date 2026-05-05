use std::path::Path;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Result, Store};

use crate::bindings::Handler;
use crate::host_state::HostState;
use crate::store::MemBucket;

/// Wraps an `Engine` and a `Linker` configured with all WireMirage host
/// imports. One `Runtime` is shared across requests; per-request state lives
/// in `HostState` instances created via `instantiate_with`.
pub struct Runtime {
    engine: Engine,
    linker: Linker<HostState>,
}

impl Runtime {
    pub fn new() -> Result<Self> {
        let engine = Engine::default();
        let mut linker: Linker<HostState> = Linker::new(&engine);
        Handler::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)?;
        Ok(Self { engine, linker })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile a component from a file (typically a `.component.wasm`
    /// produced by `wasm-tools component new`).
    pub fn load_component(&self, path: &Path) -> Result<Component> {
        Component::from_file(&self.engine, path)
    }

    /// Instantiate the component with a fresh `HostState` containing an
    /// empty route bucket and an empty group bucket. Returns the
    /// instantiated `Handler` plus the wasmtime `Store` (which owns the
    /// state and the resource handles).
    pub fn instantiate_with(
        &self,
        component: &Component,
        route_bucket: MemBucket,
        group_bucket: MemBucket,
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
    pub route: wasmtime::component::Resource<MemBucket>,
    pub group: wasmtime::component::Resource<MemBucket>,
}
