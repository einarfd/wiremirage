//! Tier-3 integration tests against a real Valkey container.
//!
//! Gated behind the `valkey-tests` feature so plain `cargo test` stays fast
//! and works on machines without Docker. CI enables the feature.
//!
//! Strategy: start one shared Valkey container (testcontainers-rs sync
//! runner, `valkey/valkey:9`) on first test access, and every test gets
//! its own scope (unique group/route prefix) so they don't interfere when
//! run in parallel.
//!
//! Container lifecycle note: the `OnceLock<SharedValkey>` below holds the
//! `Container` for the whole test binary. Rust does not run Drop on
//! statics at process exit, so the container *will* leak at the end of
//! the test run. testcontainers-rs 0.27 has no ryuk reaper to clean up
//! externally either. The justfile `test-valkey` recipe wraps the run in
//! a trap that `docker rm -f`s every container labelled
//! `org.testcontainers.managed-by=testcontainers` on exit — see the
//! justfile for the full filter rationale. Don't replace the OnceLock
//! with per-test containers without measuring the cost (35 tests × a
//! valkey-startup each adds real wall-clock).

#![cfg(feature = "valkey-tests")]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage};
use wm_host::auth::Auth;
use wm_host::bindings::wiremirage::handler::http::Request as WitRequest;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::store::tests as cases;
use wm_host::{AppState, Bucket, Runtime, Storage, router};

const COUNTER_COMPONENT_PATH: &str = env!("WM_FIXTURE_COUNTER_HANDLER_COMPONENT");

fn counter_bytes() -> Vec<u8> {
    std::fs::read(COUNTER_COMPONENT_PATH).expect("read counter fixture")
}

struct SharedValkey {
    _container: Container<GenericImage>,
    url: String,
}

fn shared() -> &'static SharedValkey {
    static SHARED: OnceLock<SharedValkey> = OnceLock::new();
    SHARED.get_or_init(|| {
        let container = GenericImage::new("valkey/valkey", "9")
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
    // `shared()` boots the container via testcontainers' SyncRunner, which
    // drives startup with its own `block_on`. That panics if the OnceLock is
    // first initialized from inside a tokio runtime. Under `cargo test` a sync
    // `#[test]` usually wins the init race first (off-runtime), so this test
    // just reads the cached value — but under nextest each test runs in its
    // own process, so this test initializes it here, on the async runtime
    // thread, and `block_on` is illegal. Force init onto a blocking thread
    // (no runtime entered); idempotent once another test has initialized it.
    let url = tokio::task::spawn_blocking(|| shared().url.clone())
        .await
        .expect("init valkey container");
    let storage = Storage::valkey(&url).expect("connect to valkey");
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/bump".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: counter_bytes(),
            source: None,
            owner_id: "test-owner".into(),
        })
        .expect("create route");
    let group = route.group_name.clone();
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

    // Three requests should yield count=1, count=2, count=3 — proving state
    // is durable in Valkey across the per-request fresh wasmtime instances.
    for expected in 1..=3 {
        let body = reqwest::Client::new()
            .get(format!("http://{addr}/bump"))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
            .send()
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

// -- Cross-replica cache invalidation (ADR-0037 item 1) ---------------------
//
// This is storage semantics, so it belongs at tier 3: the pub/sub bus is a
// no-op on the in-memory backend by construction, so the tier-2 suite can
// only cover the receiving half in-process. What needs proving against a
// real server is the part the read-through floor deliberately cannot do —
// making a *delete* on one replica stop the route serving on another. A
// stale route still matches, so those requests never reach the miss path.

/// Boot one replica over `url`, with its invalidation subscriber running
/// and confirmed subscribed. Awaiting readiness is not politeness: pub/sub
/// is at-most-once with no replay, so a message published before the
/// subscription exists is lost, and the test would flake rather than fail.
async fn boot_replica(url: &str) -> (String, Arc<RouteTable>, tokio::task::JoinHandle<()>) {
    let storage = Storage::valkey(url).expect("connect");
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let mut subscriber =
        wm_host::bus::spawn_route_invalidation_subscriber(storage.clone(), routes.clone())
            .expect("valkey storage spawns a subscriber");
    subscriber.wait_ready().await;
    let journal = Journal::new(storage);
    let app = router(AppState::new(runtime, routes.clone(), auth, journal));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (addr, routes, server)
}

async fn mock_status(addr: &str, group: &str, path: &str) -> u16 {
    reqwest::Client::new()
        .get(format!("http://{addr}{path}"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("send")
        .status()
        .as_u16()
}

/// Poll until `f` holds or the deadline passes. Invalidation is
/// asynchronous by nature, so the assertion is "converges promptly",
/// not "is already true on the next line".
async fn eventually(mut f: impl AsyncFnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn a_delete_on_one_replica_stops_the_route_serving_on_another() {
    let url = tokio::task::spawn_blocking(|| shared().url.clone())
        .await
        .expect("init valkey container");

    // A route created before either replica warms, so both have it cached.
    let storage = Storage::valkey(&url).expect("connect");
    let registry = Registry::new(storage.clone());
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["GET".into()],
            path: "/bump".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: counter_bytes(),
            source: None,
            owner_id: "test-owner".into(),
        })
        .expect("create route");
    let group = route.group_name.clone();

    let (addr_a, routes_a, _srv_a) = boot_replica(&url).await;
    let (addr_b, _routes_b, _srv_b) = boot_replica(&url).await;

    assert_eq!(mock_status(&addr_a, &group, "/bump").await, 200);
    assert_eq!(mock_status(&addr_b, &group, "/bump").await, 200);

    // Replica A serves the delete. Its own table updates synchronously;
    // B learns only because A published an invalidation.
    registry
        .delete_route(&group, route.number)
        .expect("delete route");
    routes_a.refresh_after_delete(&route.group_id, &route.id);

    assert_eq!(
        mock_status(&addr_a, &group, "/bump").await,
        404,
        "the deleting replica is correct immediately"
    );
    assert!(
        eventually(async || mock_status(&addr_b, &group, "/bump").await == 404).await,
        "replica B stopped serving the deleted route"
    );
}
