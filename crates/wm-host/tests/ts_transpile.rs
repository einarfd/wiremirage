//! Slice 58 tier-2 test: TypeScript routes get transpiled in-host
//! via pure-Rust swc and then dispatch through the shared js-engine
//! component like a JS route. No Node sidecar involved.

use std::path::PathBuf;
use std::sync::Arc;

use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage};

fn vendored_engine_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

fn runtime_with_engine() -> Option<Arc<Runtime>> {
    let engine_path = vendored_engine_path()?;
    let storage = Storage::in_memory();
    let runtime = Runtime::new(storage)
        .expect("runtime")
        .with_js_engine(&engine_path)
        .expect("engine");
    Some(Arc::new(runtime))
}

#[tokio::test]
async fn transpile_strips_typescript_type_annotations() {
    let Some(runtime) = runtime_with_engine() else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };
    let ts = r#"
        type Body = { ok: boolean };
        function handle(req: unknown, route: unknown, group: unknown): Body {
            const _ = req as { method: string };
            return { status: 200, headers: [], body: new TextEncoder().encode("ok") } as any;
        }
    "#;
    let _ = runtime;
    let ts_owned = ts.to_string();
    let js = tokio::task::spawn_blocking(move || wm_transpile::transpile(&ts_owned))
        .await
        .expect("spawn")
        .expect("transpile");
    // Type-only constructs are gone.
    assert!(
        !js.contains("type Body"),
        "type-alias should be stripped, got:\n{js}"
    );
    assert!(
        !js.contains(": Body"),
        "return-type annotation should be stripped"
    );
    assert!(!js.contains(" as { method"), "cast should be stripped");
    // Function body still present.
    assert!(js.contains("function handle"));
    assert!(js.contains("status: 200"));
}

#[tokio::test]
async fn typescript_route_creates_and_dispatches() {
    use wm_host::router;

    let Some(runtime) = runtime_with_engine() else {
        return;
    };

    // Re-create storage + auth fresh so we can reuse this runtime
    // in the api-driven create-route path.
    let storage = runtime.storage().clone();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let app = router(AppState::new(runtime, routes, auth, journal));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    // POST /api/routes with language=typescript + source. Host
    // transpiles via engine.transpile, stores as JS-shape source,
    // dispatch routes through the engine.
    let body = serde_json::json!({
        "methods": ["GET"],
        "path": "/v1/ts-handler",
        "language": "typescript",
        "source": r#"
            type Reply = { status: number; headers: [string, string][]; body: Uint8Array };
            function handle(req: unknown, _route: unknown, _group: unknown): Reply {
                const r = req as { method: string; path: string };
                const body = new TextEncoder().encode("ts: " + r.method + " " + r.path);
                return { status: 200, headers: [["content-type", "text/plain"]], body };
            }
        "#,
    });
    let create = reqwest::Client::new()
        .post(format!("http://{addr}/api/routes"))
        .header("authorization", "Bearer wmt_test")
        .json(&body)
        .send()
        .await
        .expect("create");
    let status = create.status();
    let create_body = create.text().await.unwrap_or_default();
    assert_eq!(
        status, 201,
        "create route status: {status} body: {create_body}"
    );
    let created: serde_json::Value = serde_json::from_str(&create_body).expect("json");
    let group = created["group"]["name"].as_str().expect("group name");

    // Now hit the route on the mock-traffic listener.
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/v1/ts-handler"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("dispatch");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.expect("body");
    assert_eq!(text, "ts: GET /v1/ts-handler");
    server.abort();
}

#[tokio::test]
async fn transpile_surfaces_typescript_syntax_errors() {
    let Some(runtime) = runtime_with_engine() else {
        return;
    };
    // Mismatched braces — swc's TS parser is fairly forgiving on
    // missing/extra punctuation in expression position, but
    // top-level brace mismatch is a clear parse error.
    let bad = r#"
        function handle(req) { return {
    "#;
    let _ = runtime;
    let bad_owned = bad.to_string();
    let err = tokio::task::spawn_blocking(move || wm_transpile::transpile(&bad_owned))
        .await
        .expect("spawn")
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("parse") || err.contains("transpile"),
        "error names the failure: {err}"
    );
}
