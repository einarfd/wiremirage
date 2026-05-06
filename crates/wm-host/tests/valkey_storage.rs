//! Tier-3 integration tests against a real Valkey container.
//!
//! Gated behind the `valkey-tests` feature so plain `cargo test` stays fast
//! and works on machines without Docker. CI enables the feature.
//!
//! Strategy: start one shared Valkey container (testcontainers-rs sync
//! runner, `valkey/valkey:8`) on first test access, and every test gets
//! its own scope (unique group/route prefix) so they don't interfere when
//! run in parallel.

#![cfg(feature = "valkey-tests")]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage};
use wm_host::bindings::wiremirage::handler::http::Request as WitRequest;
use wm_host::store::tests as cases;
use wm_host::{AppState, Bucket, Runtime, Storage, router};

const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");

struct SharedValkey {
    _container: Container<GenericImage>,
    url: String,
}

fn shared() -> &'static SharedValkey {
    static SHARED: OnceLock<SharedValkey> = OnceLock::new();
    SHARED.get_or_init(|| {
        let container = GenericImage::new("valkey/valkey", "8")
            .with_exposed_port(6379.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
            .start()
            .expect("start valkey container");
        let host = container.get_host().expect("get_host");
        let port = container
            .get_host_port_ipv4(6379.tcp())
            .expect("get_host_port_ipv4");
        let url = format!("redis://{host}:{port}");
        SharedValkey {
            _container: container,
            url,
        }
    })
}

fn fresh_bucket(scope: &str) -> Bucket {
    let storage = Storage::valkey(&shared().url).expect("connect to valkey");
    storage
        .route_bucket(&format!("test-{scope}"), "r")
        .expect("open route bucket")
}

// One #[test] fn per case, each with its own scope so they're isolated when
// cargo runs them in parallel.

macro_rules! decl_cases {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let mut bk = fresh_bucket(stringify!($name));
                cases::$name(&mut bk);
            }
        )*
    };
}

wm_host::storage_cases!(decl_cases);

// -- End-to-end smoke: state persists across requests over HTTP -------------

#[tokio::test]
async fn counter_persists_across_requests_via_http() {
    let storage = Storage::valkey(&shared().url).expect("connect to valkey");
    let runtime = Arc::new(Runtime::new(storage).expect("runtime"));
    let component = runtime
        .load_component(&PathBuf::from(COUNTER_COMPONENT_PATH))
        .expect("load component");
    let app = router(AppState::new(runtime, component));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });

    // Three requests should yield count=1, count=2, count=3 — proving state
    // is durable in Valkey across the per-request fresh wasmtime instances.
    for expected in 1..=3 {
        let body = reqwest::get(format!("http://{addr}/bump"))
            .await
            .expect("get")
            .text()
            .await
            .expect("body");
        assert_eq!(body, format!("count={expected}"));
    }

    server.abort();
}

// -- Direct call into the counter fixture (no HTTP layer) -------------------

#[test]
fn counter_increments_via_direct_call() {
    let storage = Storage::valkey(&shared().url).expect("connect to valkey");
    let runtime = Runtime::new(storage).expect("runtime");
    let component = runtime
        .load_component(&PathBuf::from(COUNTER_COMPONENT_PATH))
        .expect("load component");

    for expected in 1..=3 {
        let (handler, mut store, handles) = runtime
            .instantiate(&component, "counter-direct", "r")
            .expect("instantiate");
        let req = WitRequest {
            method: "GET".into(),
            path: "/bump".into(),
            matched_pattern: "*".into(),
            path_params: vec![],
            query: vec![],
            headers: vec![],
            body: vec![],
        };
        let resp = handler
            .call_handle(&mut store, &req, handles.route, handles.group)
            .expect("call handle");
        assert_eq!(resp.body, format!("count={expected}").into_bytes());
    }
}
