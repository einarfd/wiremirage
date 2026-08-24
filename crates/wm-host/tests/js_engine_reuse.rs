//! The shared-engine reuse invariant (ADR-0020): the JS engine
//! component is built once at startup and reused for every request.
//!
//! Compiling that component is the dominant cost on the engine path
//! (~4 min on a CI runner, see `runtime.rs`), so a change that rebuilds
//! it per request would be a severe regression that every behavioural
//! test still passes.
//!
//! This used to assert the shape with a stopwatch — "subsequent
//! requests are ≥1.5× faster than the first". That measured a shadow of
//! the property rather than the property: the ratio depends on runner
//! load, the absolute times are ~15 ms in debug, and it failed twice on
//! noise after already being loosened once from 3× to 1.5×. Each
//! failure skipped the image publish, because `image-build` needs
//! `test`. Counting the builds tests the same invariant as a fact, is
//! immune to how loaded the machine is, and fails with the actual
//! reason instead of a ratio.

use std::path::PathBuf;
use std::sync::Arc;

use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

fn vendored_engine_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

#[tokio::test]
async fn engine_component_is_built_once_and_reused_across_requests() {
    let Some(engine_path) = vendored_engine_path() else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Runtime::new(storage.clone())
        .expect("runtime")
        .with_js_engine(&engine_path)
        .expect("engine");
    let runtime = Arc::new(runtime);
    let registry = Arc::new(Registry::new(storage.clone()));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/perf".into(),
            language: "javascript".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: Vec::new(),
            source: Some(
                r#"
                function handle(req, route, group) {
                  return { status: 200, headers: [], body: new TextEncoder().encode("ok") };
                }
                "#
                .into(),
            ),
            owner_id: "test-owner".into(),
        })
        .expect("create");
    let group = route.group_name.clone();
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let journal = Journal::new(storage);
    // Keep a handle: `AppState` takes the Arc, and the counter has to be
    // read from the very runtime that serves the requests.
    let runtime_handle = Arc::clone(&runtime);
    let app = router(AppState::new(runtime, routes, auth, journal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    let client = reqwest::Client::new();

    // Wiring the engine in builds the component exactly once. Anything
    // above 1 here means startup itself is rebuilding it.
    let after_startup = runtime_handle.components_built();
    assert_eq!(
        after_startup, 1,
        "engine setup should build exactly one component, built {after_startup}",
    );

    // Serve enough requests that a per-request rebuild could not hide.
    const REQUESTS: u32 = 10;
    for i in 0..REQUESTS {
        let resp = client
            .post(format!("http://{addr}/v1/perf"))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("request {i} failed: {e}"));
        assert_eq!(resp.status(), 200, "request {i}");
        assert_eq!(resp.text().await.expect("body"), "ok", "request {i}");
    }

    // The invariant. If dispatch ever instantiates from a freshly built
    // component instead of the shared `Arc<Component>`, this is
    // 1 + REQUESTS rather than 1.
    let after_requests = runtime_handle.components_built();
    assert_eq!(
        after_requests, after_startup,
        "the engine component must be reused across requests, not rebuilt: \
         {after_startup} built at startup, {after_requests} after {REQUESTS} \
         requests — a per-request rebuild means the shared component was lost",
    );

    server.abort();
}
