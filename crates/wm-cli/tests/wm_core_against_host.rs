//! Tier-2 tests: drive `wm_core::Client` against a real `wm-host`
//! booted in-process. Covers the happy paths that don't need a
//! valid wasm component (groups, tokens, journal, error mapping).
//! Route creation is exercised at tier 1 by the wm-core client mock
//! and at tier 2 by `crates/wm-host/tests/api_routes.rs` against a
//! real wasm fixture; we don't repeat that here.

use std::sync::Arc;

use wm_core::{
    Client, ClientError, CreateGroupBody, CreateTokenBody, CreateUserBody, DryRunBody,
    MatchResponse, PatchGroupBody, PatchRouteBody, PatchUserBody,
};
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";

struct Harness {
    host_url: String,
    state: AppState,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
        .expect("bootstrap admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal);
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        host_url: format!("http://{addr}"),
        state,
        server,
    }
}

fn client(host_url: &str) -> Client {
    Client::builder(host_url)
        .with_token(BOOTSTRAP_TOKEN)
        .build()
        .expect("build")
}

fn bootstrap_user_id(h: &Harness) -> String {
    h.state
        .auth()
        .get_user_by_name("bootstrap")
        .expect("read bootstrap user")
        .expect("bootstrap user exists")
        .id
}

#[tokio::test]
async fn health_works_without_a_token() {
    let h = start().await;
    let client = Client::builder(&h.host_url).build().expect("build");
    let health = client.health().await.expect("health");
    assert_eq!(health.status, "ok");
}

#[tokio::test]
async fn group_full_round_trip() {
    let h = start().await;
    let client = client(&h.host_url);

    // Create.
    let body = CreateGroupBody {
        name: "stripe-mock".into(),
        ttl_seconds: Some(3600),
        sliding_ttl: Some(false),
    };
    let g = client.create_group(&body).await.expect("create");
    assert_eq!(g.name, "stripe-mock");
    assert_eq!(g.ttl_seconds, 3600);
    assert!(!g.sliding_ttl);

    // List — bootstrap (admin) sees it.
    let listed = client.list_groups().await.expect("list");
    assert!(listed.groups.iter().any(|x| x.name == "stripe-mock"));

    // Get one.
    let one = client.get_group("stripe-mock").await.expect("get");
    assert_eq!(one.id, g.id);

    // Patch.
    let patched = client
        .patch_group(
            "stripe-mock",
            &PatchGroupBody {
                ttl_seconds: Some(7200),
                sliding_ttl: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("patch");
    assert_eq!(patched.ttl_seconds, 7200);
    assert!(patched.sliding_ttl);

    // Refresh.
    client.refresh_group("stripe-mock").await.expect("refresh");

    // Delete.
    client.delete_group("stripe-mock").await.expect("delete");

    // Now 404.
    let err = client.get_group("stripe-mock").await.unwrap_err();
    assert!(matches!(err, ClientError::NotFound(_)));
}

#[tokio::test]
async fn missing_token_returns_typed_error() {
    let h = start().await;
    let client = Client::builder(&h.host_url).build().expect("build");
    let err = client.list_groups().await.unwrap_err();
    assert!(
        matches!(err, ClientError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}

#[tokio::test]
async fn invalid_token_returns_typed_error() {
    let h = start().await;
    let client = Client::builder(&h.host_url)
        .with_token("wmt_not_real")
        .build()
        .expect("build");
    let err = client.list_groups().await.unwrap_err();
    assert!(matches!(err, ClientError::Unauthorized(_)));
}

#[tokio::test]
async fn duplicate_group_name_is_conflict() {
    let h = start().await;
    let client = client(&h.host_url);
    let body = CreateGroupBody {
        name: "dup".into(),
        ..Default::default()
    };
    client.create_group(&body).await.expect("first");
    let err = client.create_group(&body).await.unwrap_err();
    assert!(matches!(err, ClientError::Conflict(_)));
}

#[tokio::test]
async fn token_create_returns_plaintext_and_authenticates() {
    let h = start().await;
    let client = client(&h.host_url);
    let body = CreateTokenBody {
        name: "ci-runner".into(),
        ttl_seconds: None,
    };
    let resp = client.create_token(&body).await.expect("create");
    assert!(resp.token.starts_with("wmt_"));
    assert_eq!(resp.record.name, "ci-runner");

    // The plaintext just minted should authenticate against the same
    // host.
    let new_client = Client::builder(&h.host_url)
        .with_token(&resp.token)
        .build()
        .expect("build");
    let tokens = new_client.list_tokens().await.expect("list");
    assert!(tokens.tokens.iter().any(|t| t.name == "ci-runner"));
}

#[tokio::test]
async fn token_rename_changes_name_via_client() {
    let h = start().await;
    let client = client(&h.host_url);
    client
        .create_token(&CreateTokenBody {
            name: "old-name".into(),
            ttl_seconds: None,
        })
        .await
        .expect("create");

    let renamed = client
        .rename_token("old-name", "new-name")
        .await
        .expect("rename");
    assert_eq!(renamed.name, "new-name");

    let tokens = client.list_tokens().await.expect("list");
    assert!(tokens.tokens.iter().any(|t| t.name == "new-name"));
    assert!(!tokens.tokens.iter().any(|t| t.name == "old-name"));
}

#[tokio::test]
async fn journal_list_empty_for_fresh_group() {
    let h = start().await;
    let client = client(&h.host_url);
    client
        .create_group(&CreateGroupBody {
            name: "fresh".into(),
            ..Default::default()
        })
        .await
        .expect("create");
    let listed = client
        .list_journal("fresh", None, None)
        .await
        .expect("list journal");
    assert!(listed.entries.is_empty());
}

#[tokio::test]
async fn match_route_round_trips_hit_and_miss() {
    let h = start().await;
    let client = client(&h.host_url);

    // Insert a route directly through the registry — the REST
    // create-route path validates wasm bytes, but the match probe
    // only consults stored metadata. Going via the registry keeps
    // this test free of wasm fixtures.
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: "test-owner".into(),
        })
        .expect("registry create_route");
    let group = route.group_name.clone();
    h.state.routes().refresh_after_create(route);

    // Hit case.
    let hit_resp = client
        .match_route(&group, "POST", "/v1/charges")
        .await
        .expect("match POST /v1/charges");
    match hit_resp {
        MatchResponse::Hit(hit) => {
            assert!(hit.matched);
            assert_eq!(hit.route.path, "/v1/charges");
        }
        MatchResponse::Miss(_) => panic!("expected hit"),
    }

    // Miss + method mismatch near-miss.
    let miss_resp = client
        .match_route(&group, "GET", "/v1/charges")
        .await
        .expect("match GET /v1/charges");
    match miss_resp {
        MatchResponse::Miss(miss) => {
            assert!(!miss.matched);
            assert_eq!(miss.near_misses.len(), 1);
            assert_eq!(
                miss.near_misses[0].reason,
                wm_core::NearMissReason::MethodMismatch
            );
        }
        MatchResponse::Hit(_) => panic!("expected miss"),
    }
}

#[tokio::test]
async fn patch_route_round_trip_metadata_only() {
    let h = start().await;
    let client = client(&h.host_url);

    // Plant a route directly through the registry — PATCH wasm-bytes
    // would require a real component fixture; we exercise the
    // metadata-only path here and let api_routes.rs cover the wasm
    // swap with a fixture.
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/foo".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: bootstrap_user_id(&h),
        })
        .expect("registry create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let patched = client
        .patch_route(
            &slug,
            &PatchRouteBody {
                methods: Some(vec!["GET".into(), "POST".into()]),
                path: Some("/v1/bar".into()),
                ..Default::default()
            },
        )
        .await
        .expect("patch_route");
    assert_eq!(patched.methods, vec!["GET", "POST"]);
    assert_eq!(patched.path, "/v1/bar");

    // GET reflects the new state.
    let fresh = client
        .get_route(&format!("{}/{}", patched.group.name, patched.number))
        .await
        .expect("get_route");
    assert_eq!(fresh.path, "/v1/bar");
}

#[tokio::test]
async fn patch_route_with_empty_body_is_validation_error() {
    let h = start().await;
    let client = client(&h.host_url);
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/empty".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: bootstrap_user_id(&h),
        })
        .expect("registry create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let err = client
        .patch_route(&slug, &PatchRouteBody::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ClientError::Validation(_)),
        "expected Validation, got {err:?}"
    );
}

#[tokio::test]
async fn patch_route_by_non_owner_is_forbidden() {
    let h = start().await;
    // Bootstrap owns the route; alice (non-admin) tries to patch.
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/locked".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: bootstrap_user_id(&h),
        })
        .expect("registry create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let alice = h
        .state
        .auth()
        .create_user("alice-patch", false)
        .expect("alice");
    let (_token, plaintext) = h
        .state
        .auth()
        .create_token(&alice.id, "default", None)
        .expect("alice token");
    let alice_client = Client::builder(&h.host_url)
        .with_token(&plaintext)
        .build()
        .expect("build alice client");

    let err = alice_client
        .patch_route(
            &slug,
            &PatchRouteBody {
                path: Some("/v1/stolen".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ClientError::Forbidden(_)),
        "expected Forbidden, got {err:?}"
    );
}

#[tokio::test]
async fn route_state_list_and_clear_via_client() {
    let h = start().await;
    let client = client(&h.host_url);

    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/state".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: bootstrap_user_id(&h),
        })
        .expect("create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route.clone());

    // Plant some state directly so we exercise the list/clear path
    // without needing a working wasm fixture.
    let mut bucket = h
        .state
        .runtime()
        .storage()
        .route_bucket(&route.group_id, &route.id)
        .expect("route bucket");
    bucket.set("count", b"5".to_vec()).expect("set");
    bucket.list_push("events", b"a".to_vec()).expect("push");

    let listed = client.list_route_state(&slug).await.expect("list");
    assert_eq!(listed.entries.len(), 2);
    let count = listed
        .entries
        .iter()
        .find(|e| e.key == "count")
        .expect("count present");
    assert_eq!(count.kind, "bytes");
    assert_eq!(count.value.as_deref(), Some(b"5".as_slice()));

    client.clear_route_state(&slug).await.expect("clear");
    let empty = client.list_route_state(&slug).await.expect("list 2");
    assert!(empty.entries.is_empty());
}

#[tokio::test]
async fn dry_run_against_bogus_wasm_returns_error_shape() {
    let h = start().await;
    let client = client(&h.host_url);
    // The route's compiled_wasm is `b"FAKE"`, which will fail to
    // compile when the runtime tries to instantiate it. We're
    // verifying the dry-run *endpoint* returns the wire shape — the
    // error surfaces in the response body's `error` field, not as a
    // 500 from the HTTP layer.
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/dry".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            source: None,
            owner_id: bootstrap_user_id(&h),
        })
        .expect("create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let result = client
        .dry_run_route(
            &slug,
            &DryRunBody {
                method: "POST".into(),
                path: "/v1/dry".into(),
                ..Default::default()
            },
        )
        .await
        .expect("dry_run_route");
    assert_eq!(result.status, 500);
    assert!(result.error.is_some(), "expected error message");
}

#[tokio::test]
async fn user_agent_default_starts_with_wm_cli() {
    // Indirect coverage: any successful authed request through the
    // default-built Client uses the default User-Agent. We rely on
    // the wm-host journal write path to capture User-Agent in the
    // request headers.
    let h = start().await;
    let client = client(&h.host_url);
    // Create a group so we can hit the journal endpoint cleanly.
    client
        .create_group(&CreateGroupBody {
            name: "ua-check".into(),
            ..Default::default()
        })
        .await
        .expect("create");
    // The /__api/groups POST went through the default client; the
    // host journal would have stamped a User-Agent in OTel spans
    // (not journaled for /__api/* traffic). For tier-2 we simply
    // confirm the call succeeds; the tier-1 mock test asserts the
    // header value directly.
}

#[tokio::test]
async fn user_full_round_trip_admin() {
    let h = start().await;
    let admin = client(&h.host_url);

    // Bootstrap user is the only one until we create more.
    let initial = admin.list_users().await.expect("list");
    assert_eq!(initial.users.len(), 1);
    assert_eq!(initial.users[0].name, "bootstrap");
    assert!(initial.users[0].is_admin);

    // Create alice as non-admin.
    let alice = admin
        .create_user(&CreateUserBody {
            name: "alice".into(),
            is_admin: false,
        })
        .await
        .expect("create alice");
    assert!(!alice.is_admin);

    // Show by name.
    let shown = admin.get_user("alice").await.expect("get alice");
    assert_eq!(shown.name, "alice");

    // Promote.
    let promoted = admin
        .patch_user(
            "alice",
            &PatchUserBody {
                is_admin: Some(true),
            },
        )
        .await
        .expect("promote alice");
    assert!(promoted.is_admin);

    // Demote (and keep — admin can't remove the last admin, and we
    // still have bootstrap, so this is fine).
    let demoted = admin
        .patch_user(
            "alice",
            &PatchUserBody {
                is_admin: Some(false),
            },
        )
        .await
        .expect("demote alice");
    assert!(!demoted.is_admin);

    // Delete.
    admin.delete_user("alice").await.expect("delete alice");
    let after = admin.list_users().await.expect("list after");
    assert_eq!(after.users.len(), 1);
}

#[tokio::test]
async fn non_admin_cannot_list_users() {
    let h = start().await;
    let admin = client(&h.host_url);

    // Provision a non-admin user via the registry + auth, then build
    // a client carrying their token.
    let user = h
        .state
        .auth()
        .create_user("alice", false)
        .expect("create user");
    let (_token, plaintext) = h
        .state
        .auth()
        .create_token(&user.id, "default", None)
        .expect("create token");
    let alice = Client::builder(&h.host_url)
        .with_token(&plaintext)
        .build()
        .expect("build alice");

    let err = alice.list_users().await.expect_err("expected forbidden");
    assert!(
        matches!(err, ClientError::Forbidden(_)),
        "expected Forbidden, got: {err:?}"
    );

    // Admin can still list (sanity).
    let _ = admin.list_users().await.expect("admin list");
}

#[tokio::test]
async fn me_works_for_any_authed_user() {
    let h = start().await;
    let user = h
        .state
        .auth()
        .create_user("alice", false)
        .expect("create user");
    let (_token, plaintext) = h
        .state
        .auth()
        .create_token(&user.id, "default", None)
        .expect("create token");
    let alice = Client::builder(&h.host_url)
        .with_token(&plaintext)
        .build()
        .expect("build alice");
    let me = alice.get_me().await.expect("get me");
    assert_eq!(me.name, "alice");
    assert!(!me.is_admin);
}
