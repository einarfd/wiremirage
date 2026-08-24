//! Slice 56 spike test: load the vendored `js-engine.wasm` component
//! built from `compiler/js-engine/`, provide a stub `get-source`
//! that returns a known JS handler, instantiate the engine, call
//! `handle`, verify the response shape.
//!
//! This is the proof that ADR-0020's componentize-js shim approach
//! works end-to-end at the wasmtime level. Slice 57 wires it into
//! the real route-dispatch path; slice 58 swaps the sidecar for
//! in-host TS→JS transpile.
//!
//! Test inputs are deliberately small (a one-line JS handler) so a
//! failure mode like "engine couldn't parse / call the user
//! handle" is unambiguous.
//!
//! Skipped automatically when the vendored binary isn't present —
//! re-runners on a fresh checkout don't need node installed.

use std::sync::Mutex;

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wm_host::Runtime;
use wm_host::bindings::engine_bindings::Engine as EngineWorld;
use wm_host::bindings::engine_bindings::wiremirage::handler::callback::Host as CallbackHost;
use wm_host::bindings::engine_bindings::wiremirage::handler::clock::Host as ClockHost;
use wm_host::bindings::engine_bindings::wiremirage::handler::engine_host::Host as EngineHostTrait;
use wm_host::bindings::engine_bindings::wiremirage::handler::http::{
    Host as HttpHost, Request as WitRequest,
};
use wm_host::bindings::engine_bindings::wiremirage::handler::log::{
    Host as LogHost, Level as LogLevel,
};
use wm_host::bindings::engine_bindings::wiremirage::handler::response_stream::Host as ResponseStreamHost;
use wm_host::bindings::engine_bindings::wiremirage::handler::store::{
    Host as StoreHost, HostBucket,
};
use wm_host::store::{Bucket, Storage};

// Pre-slice-59 this pointed at `crates/wm-host/vendored/js-engine.wasm`.
// The engine is now built at cargo build time (ADR-0020 slice C); build.rs
// stamps the OUT_DIR path into WM_JS_ENGINE_WASM.

/// Minimal host state for the engine. Owns the resource table for
/// `store.bucket`, captures any logs the engine emits (it shouldn't,
/// but log is part of the world), and holds the per-request JS
/// source the engine asks for.
struct EngineState {
    table: ResourceTable,
    /// Plaintext JS source `get-source` returns.
    source: String,
    /// Any messages the engine's `log.emit` calls produced. Captured
    /// for visibility; not asserted on here.
    logs: Mutex<Vec<String>>,
}

impl EngineState {
    fn new(source: String) -> Self {
        Self {
            table: ResourceTable::new(),
            source,
            logs: Mutex::new(Vec::new()),
        }
    }

    fn push_bucket(&mut self, bucket: Bucket) -> wasmtime::component::Resource<Bucket> {
        self.table.push(bucket).expect("push bucket")
    }
}

impl EngineHostTrait for EngineState {
    fn get_source(&mut self) -> wasmtime::Result<String> {
        Ok(self.source.clone())
    }
}

impl HttpHost for EngineState {}
impl StoreHost for EngineState {}

// Streaming-response stub (ADR-0022) — the spike test doesn't exercise
// streaming; it just needs the trait satisfied so the engine world links.
impl ResponseStreamHost for EngineState {
    fn start(&mut self, _status: u16, _headers: Vec<(String, String)>) -> wasmtime::Result<()> {
        Ok(())
    }
    fn write_chunk(&mut self, _bytes: Vec<u8>) -> wasmtime::Result<bool> {
        Ok(true)
    }
    fn finish(&mut self) -> wasmtime::Result<()> {
        Ok(())
    }
}
impl CallbackHost for EngineState {
    fn schedule(
        &mut self,
        _url: String,
        _method: String,
        _headers: Vec<(String, String)>,
        _body: Vec<u8>,
        _delay_ms: u64,
    ) -> wasmtime::Result<std::result::Result<(), String>> {
        // The spike test doesn't exercise callbacks; accept and discard.
        Ok(Ok(()))
    }
}
impl LogHost for EngineState {
    fn emit(&mut self, level: LogLevel, message: String) -> wasmtime::Result<()> {
        self.logs
            .lock()
            .unwrap()
            .push(format!("{level:?}: {message}"));
        Ok(())
    }
}

// Minimal clock stub — the spike test doesn't exercise clock semantics
// (those have dedicated tests against the real host impl), it just
// needs the trait to be satisfied so the engine world can link.
impl ClockHost for EngineState {
    fn sleep(&mut self, _ms: u64) -> wasmtime::Result<()> {
        Ok(())
    }
    fn wall_time_ms(&mut self) -> wasmtime::Result<u64> {
        Ok(0)
    }
    fn monotonic_ms(&mut self) -> wasmtime::Result<u64> {
        Ok(0)
    }
}

impl HostBucket for EngineState {
    fn get(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
    ) -> wasmtime::Result<Option<Vec<u8>>> {
        Ok(self.table.get_mut(&self_)?.get(&key)?)
    }
    fn set(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        value: Vec<u8>,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.set(&key, value)?)
    }
    fn delete(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.delete(&key)?)
    }
    fn incr(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        by: i64,
    ) -> wasmtime::Result<i64> {
        Ok(self.table.get_mut(&self_)?.incr(&key, by)?)
    }
    fn list_keys(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        prefix: Option<String>,
    ) -> wasmtime::Result<Vec<String>> {
        Ok(self.table.get_mut(&self_)?.list_keys(prefix.as_deref())?)
    }
    fn list_push(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        value: Vec<u8>,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.list_push(&key, value)?)
    }
    fn list_pop(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
    ) -> wasmtime::Result<Option<Vec<u8>>> {
        Ok(self.table.get_mut(&self_)?.list_pop(&key)?)
    }
    fn list_range(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        start: i64,
        stop: i64,
    ) -> wasmtime::Result<Vec<Vec<u8>>> {
        Ok(self.table.get_mut(&self_)?.list_range(&key, start, stop)?)
    }
    fn list_length(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
    ) -> wasmtime::Result<u64> {
        Ok(self.table.get_mut(&self_)?.list_length(&key)?)
    }
    fn hash_get(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        field: String,
    ) -> wasmtime::Result<Option<Vec<u8>>> {
        Ok(self.table.get_mut(&self_)?.hash_get(&key, &field)?)
    }
    fn hash_set(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        field: String,
        value: Vec<u8>,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.hash_set(&key, &field, value)?)
    }
    fn hash_delete(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        field: String,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.hash_delete(&key, &field)?)
    }
    fn hash_keys(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
    ) -> wasmtime::Result<Vec<String>> {
        Ok(self.table.get_mut(&self_)?.hash_keys(&key)?)
    }
    fn set_add(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        member: String,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.set_add(&key, &member)?)
    }
    fn set_remove(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        member: String,
    ) -> wasmtime::Result<()> {
        Ok(self.table.get_mut(&self_)?.set_remove(&key, &member)?)
    }
    fn set_contains(
        &mut self,
        self_: wasmtime::component::Resource<Bucket>,
        key: String,
        member: String,
    ) -> wasmtime::Result<bool> {
        Ok(self.table.get_mut(&self_)?.set_contains(&key, &member)?)
    }
    fn drop(&mut self, rep: wasmtime::component::Resource<Bucket>) -> wasmtime::Result<()> {
        let _ = self.table.delete(rep)?;
        Ok(())
    }
}

/// The engine `Component` plus the `Engine` it belongs to, obtained
/// through `Runtime`'s cross-process artifact cache.
///
/// These tests drive the engine world directly with their own linker and
/// host-import implementations, but they used to build their own
/// `Engine` and `Component::from_file` it — a full Cranelift compile of
/// the ~12 MB engine in every test process, every run. That cannot hit
/// the shared `.cwasm` cache: an artifact only deserializes on an
/// `Engine` whose `Config` matches the one that produced it, and these
/// built their own `Config`. It made these the two slowest tests in the
/// suite (>240 s each on CI).
///
/// Borrowing `Runtime`'s engine and component keeps the test's own
/// linker and state while paying the compile at most once per machine.
/// The store setup below has to match that `Config`: it enables fuel
/// and epoch interruption, so a store that sets neither traps
/// immediately.
fn cached_engine_component() -> Option<(Engine, std::sync::Arc<Component>)> {
    let path = vendored_engine_path()?;
    let runtime = Runtime::new(Storage::in_memory())
        .expect("runtime")
        .with_js_engine(&path)
        .expect("load js-engine.wasm");
    let component = runtime.js_engine_component().expect("engine component");
    Some((runtime.engine().clone(), component))
}

/// Fuel and epoch budgets generous enough that neither fires; the
/// engine `Config` requires both to be set.
fn prepare_store(store: &mut Store<EngineState>) {
    store.set_fuel(u64::MAX).expect("set fuel");
    store.set_epoch_deadline(u64::MAX);
}

fn vendored_engine_path() -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

#[test]
fn js_engine_runs_user_handler_via_get_source_host_import() {
    let Some((wasm_engine, component)) = cached_engine_component() else {
        // Vendored binary not present on this checkout; build.rs writes it.
        eprintln!(
            "skipping: js-engine.wasm not present at {} — build.rs should have produced it",
            env!("WM_JS_ENGINE_WASM")
        );
        return;
    };

    let mut linker: Linker<EngineState> = Linker::new(&wasm_engine);
    EngineWorld::add_to_linker::<_, wasmtime::component::HasSelf<EngineState>>(&mut linker, |s| s)
        .expect("add_to_linker");

    // Per-request setup: known JS source, in-memory buckets.
    let storage = Storage::in_memory();
    let route_bucket = storage
        .route_bucket("group", "route")
        .expect("route bucket");
    let group_bucket = storage.group_bucket("group").expect("group bucket");
    let source = r#"
        function handle(req, route, group) {
          return {
            status: 200,
            headers: [["content-type", "text/plain; charset=utf-8"]],
            body: new TextEncoder().encode("hello from shared engine: " + req.method + " " + req.path),
          };
        }
    "#;
    let mut state = EngineState::new(source.into());
    let route_handle = state.push_bucket(route_bucket);
    let group_handle = state.push_bucket(group_bucket);
    let mut store = Store::new(&wasm_engine, state);
    prepare_store(&mut store);

    let engine_world =
        EngineWorld::instantiate(&mut store, &component, &linker).expect("instantiate js-engine");

    let req = WitRequest {
        method: "POST".into(),
        path: "/v1/hello".into(),
        matched_pattern: "/v1/hello".into(),
        path_params: vec![],
        query: vec![],
        headers: vec![],
        body: vec![],
    };
    let response = engine_world
        .call_handle(&mut store, &req, route_handle, group_handle)
        .expect("call handle");

    assert_eq!(response.status, 200);
    let body_text = String::from_utf8(response.body).expect("utf8 body");
    assert_eq!(body_text, "hello from shared engine: POST /v1/hello");
    let content_type = response
        .headers
        .iter()
        .find_map(|(k, v)| (k.eq_ignore_ascii_case("content-type")).then(|| v.clone()))
        .expect("content-type header");
    assert!(content_type.starts_with("text/plain"));
}

#[test]
fn js_engine_surfaces_handler_throw_as_500() {
    let Some((wasm_engine, component)) = cached_engine_component() else {
        // Vendored binary not present on this checkout; build.rs writes it.
        eprintln!(
            "skipping: js-engine.wasm not present at {} — build.rs should have produced it",
            env!("WM_JS_ENGINE_WASM")
        );
        return;
    };

    let mut linker: Linker<EngineState> = Linker::new(&wasm_engine);
    EngineWorld::add_to_linker::<_, wasmtime::component::HasSelf<EngineState>>(&mut linker, |s| s)
        .expect("add_to_linker");

    let storage = Storage::in_memory();
    let route_bucket = storage.route_bucket("group", "route").expect("rb");
    let group_bucket = storage.group_bucket("group").expect("gb");
    let source = r#"
        function handle(req, route, group) {
          throw new Error("intentional explosion");
        }
    "#;
    let mut state = EngineState::new(source.into());
    let r = state.push_bucket(route_bucket);
    let g = state.push_bucket(group_bucket);
    let mut store = Store::new(&wasm_engine, state);
    prepare_store(&mut store);
    let engine_world = EngineWorld::instantiate(&mut store, &component, &linker).expect("inst");

    let req = WitRequest {
        method: "GET".into(),
        path: "/x".into(),
        matched_pattern: "/x".into(),
        path_params: vec![],
        query: vec![],
        headers: vec![],
        body: vec![],
    };
    let response = engine_world
        .call_handle(&mut store, &req, r, g)
        .expect("call");
    assert_eq!(response.status, 500);
    let body = String::from_utf8(response.body).expect("utf8");
    assert!(
        body.contains("intentional explosion"),
        "error body surfaces the message: {body}"
    );
}
