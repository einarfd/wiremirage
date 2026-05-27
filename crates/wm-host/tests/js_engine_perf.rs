//! Slice 57 sanity check: confirm the shared-engine path has the
//! expected first-call-expensive / steady-state-cheap shape.
//!
//! Not a real benchmark (debug mode runs are 15× release) — exists
//! so a future change that breaks the amortization shows up loudly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

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
async fn engine_dispatch_amortizes_jit_cost_across_requests() {
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
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let journal = Journal::new(storage);
    let app = router(AppState::new(runtime, routes, auth, journal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    let client = reqwest::Client::new();

    // First request eats the JIT cost. We don't gate on a specific
    // wall time (debug vs release vary 15×) — just sanity-check
    // that the steady-state requests are dramatically cheaper than
    // the first.
    let t0 = Instant::now();
    client
        .post(format!("http://{addr}/v1/perf"))
        .send()
        .await
        .expect("first")
        .text()
        .await
        .ok();
    let first = t0.elapsed();

    // Steady-state: 10 sequential requests, averaged. The total of
    // these should be substantially less than the first request's
    // cost. (10 rather than 5 to dampen per-request timing noise on
    // shared CI runners.)
    const STEADY_N: u32 = 10;
    let t1 = Instant::now();
    for _ in 0..STEADY_N {
        let resp = client
            .post(format!("http://{addr}/v1/perf"))
            .send()
            .await
            .expect("subsequent");
        assert_eq!(resp.status(), 200);
    }
    let steady = t1.elapsed();
    let per_subsequent = steady / STEADY_N;

    eprintln!(
        "js engine perf: first={first:?} {STEADY_N}-total={steady:?} per-subsequent={per_subsequent:?}"
    );

    // The contract this test enforces: subsequent requests are
    // meaningfully faster than the first, because the shared
    // `Arc<Component>` lets subsequent instantiations reuse the JIT'd
    // code instead of recompiling. If someone accidentally kills that
    // sharing, every request recompiles and the ratio collapses to
    // ~1×. We gate at 1.5× rather than a tight 3×: a broken cache
    // (~1×) is caught decisively, while the headroom keeps the test
    // from flaking when a fast/contended CI runner makes the absolute
    // times small and noisy (the original 3× tripped at an observed
    // 2.6× on a fast runner).
    assert!(
        per_subsequent.saturating_mul(3) < first.saturating_mul(2),
        "subsequent requests should be ≥1.5× faster than the first — a \
         collapse toward 1× means the component cache was lost \
         (first={first:?}, per_subsequent={per_subsequent:?})",
    );

    server.abort();
}
