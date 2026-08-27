//! Tier-2 tests for the ADR-0037 item 1 read-through floor.
//!
//! Two `AppState`s over **one** `Storage` stand in for two replicas
//! behind a load balancer. That is a faithful model of the failure:
//! each replica warms its own route table at startup and refreshes it
//! only for API calls it served itself, so a route created through A is
//! invisible to B until something re-reads storage.
//!
//! `storage-model.md` has always promised that a committed route
//! creation is reachable from any host backed by the same storage. Until
//! the read-through existed that promise was false — the lazy
//! populate-on-miss it relied on only ever covered the *compiled
//! artifact* caches, which are reached after a route record has already
//! been found in the process-local vector, so they could never rescue a
//! record the vector had never seen.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewGroup, NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

fn echo_wasm() -> Vec<u8> {
    std::fs::read(ECHO_COMPONENT_PATH).expect("read echo fixture")
}

struct Replica {
    addr: String,
    routes: Arc<RouteTable>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Boot one replica over the shared storage. `revalidate_interval` is
/// the read-through rate limit; tests that want the reload to happen
/// deterministically pass zero rather than sleeping out the default.
async fn boot(storage: &Storage, revalidate_interval: Duration) -> Replica {
    let auth = Auth::new(storage.clone());
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm_with_revalidate_interval(
        registry,
        runtime.engine().clone(),
        revalidate_interval,
    )
    .expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes.clone(), auth, journal);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Replica {
        addr,
        routes,
        server,
    }
}

/// Send mock traffic to a replica, addressed to `group`'s subdomain.
async fn mock_get(replica: &Replica, group: &str, path: &str) -> reqwest::Response {
    Client::new()
        .get(format!("http://{}{}", replica.addr, path))
        .header("host", format!("{group}.localhost"))
        .send()
        .await
        .expect("send")
}

fn create_route(registry: &Registry, group: &str, path: &str) {
    registry
        .create_route(NewRoute {
            group: Some(group.into()),
            methods: vec!["GET".into()],
            path: path.into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: echo_wasm(),
            source: None,
            owner_id: "owner-1".into(),
        })
        .expect("create route");
}

#[tokio::test]
async fn a_route_created_on_one_replica_is_reachable_on_another() {
    let storage = Storage::in_memory();
    let registry = Registry::new(storage.clone());
    registry
        .create_group(NewGroup {
            name: "shared".into(),
            ttl_seconds: Some(3600),
            sliding_ttl: None,
            owner_id: "owner-1".into(),
        })
        .expect("group");

    // Both replicas warm their tables with the group present and empty.
    let a = boot(&storage, Duration::ZERO).await;
    let b = boot(&storage, Duration::ZERO).await;

    // Replica A serves the create; only A's table learns about it.
    create_route(&registry, "shared", "/v1/charges");
    a.routes.refresh_after_create(
        registry
            .get_route_by_slug("shared", 1)
            .expect("read back the route"),
    );

    assert_eq!(
        mock_get(&a, "shared", "/v1/charges")
            .await
            .status()
            .as_u16(),
        200,
        "the replica that served the create obviously has it"
    );

    // B never saw the create. Before the read-through this was a 404
    // and an unmatched journal entry — the common agent workflow
    // failing on roughly (N-1)/N of requests.
    assert_eq!(
        mock_get(&b, "shared", "/v1/charges")
            .await
            .status()
            .as_u16(),
        200,
        "replica B resolves it by reloading the group from storage"
    );
}

#[tokio::test]
async fn the_read_through_is_rate_limited_per_group() {
    // The rate limit is load-bearing: unmatched traffic is exactly the
    // traffic that misses, so an unbounded read-through would let junk
    // traffic amplify into a storage read per request.
    let storage = Storage::in_memory();
    let registry = Registry::new(storage.clone());
    registry
        .create_group(NewGroup {
            name: "slow".into(),
            ttl_seconds: Some(3600),
            sliding_ttl: None,
            owner_id: "owner-1".into(),
        })
        .expect("group");

    // A long interval: the first miss consumes the group's slot and no
    // later miss may reload until it elapses.
    let r = boot(&storage, Duration::from_secs(3600)).await;

    // First miss — reloads (finds nothing), and burns the slot.
    assert_eq!(
        mock_get(&r, "slow", "/v1/charges").await.status().as_u16(),
        404
    );

    // Now create the route directly in storage, telling no replica.
    create_route(&registry, "slow", "/v1/charges");

    // Still 404: the rate limit declines to reload again this soon.
    // This is the deliberate trade — bounded staleness in exchange for
    // a bound on storage reads under junk traffic.
    assert_eq!(
        mock_get(&r, "slow", "/v1/charges").await.status().as_u16(),
        404,
        "the second miss is inside the interval, so no reload happens"
    );
}

#[tokio::test]
async fn an_unknown_subdomain_never_triggers_a_reload() {
    // Unknown hosts are attacker-controlled, so they must not reach the
    // revalidation path at all — otherwise they could both amplify
    // storage reads and grow the rate-limit map without bound.
    let storage = Storage::in_memory();
    let r = boot(&storage, Duration::ZERO).await;

    let resp = mock_get(&r, "no-such-group", "/anything").await;
    assert_eq!(resp.status().as_u16(), 404);
    assert!(
        resp.text().await.unwrap().contains("no such group"),
        "resolved as an unknown group, short of the read-through"
    );
}

#[tokio::test]
async fn a_deleted_route_still_serves_until_invalidation_lands() {
    // Documents the read-through's deliberate limit, so the gap is a
    // known one rather than a surprise: a stale route still *matches*,
    // so the request never reaches the miss path where revalidation
    // lives. Closing this is the job of the pub/sub invalidation that
    // lands on top — see ADR-0037.
    let storage = Storage::in_memory();
    let registry = Registry::new(storage.clone());
    registry
        .create_group(NewGroup {
            name: "doomed".into(),
            ttl_seconds: Some(3600),
            sliding_ttl: None,
            owner_id: "owner-1".into(),
        })
        .expect("group");
    create_route(&registry, "doomed", "/gone");

    let a = boot(&storage, Duration::ZERO).await;
    let b = boot(&storage, Duration::ZERO).await;
    assert_eq!(mock_get(&b, "doomed", "/gone").await.status().as_u16(), 200);

    // A serves the delete; B's table keeps the route.
    let route = registry.get_route_by_slug("doomed", 1).expect("route");
    registry.delete_route("doomed", 1).expect("delete");
    a.routes.refresh_after_delete(&route.group_id, &route.id);

    assert_eq!(
        mock_get(&a, "doomed", "/gone").await.status().as_u16(),
        404,
        "the deleting replica is correct immediately"
    );
    assert_eq!(
        mock_get(&b, "doomed", "/gone").await.status().as_u16(),
        200,
        "B still serves it — the known limit of a read-through floor"
    );
}
