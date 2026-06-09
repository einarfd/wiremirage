//! Tier-2 smoke tests for the slice-22 list pages (Groups + Routes).
//!
//! Spins up the host in-process, primes the registry with a handful
//! of groups + routes owned by two users, then drives the rendered
//! HTML for the standard list-page behaviours: filter echo, owner
//! scoping, sort-toggle headers, pagination, and the bad-filter
//! error path.

use std::sync::Arc;

use reqwest::Client;
use reqwest::redirect::Policy;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::{NewGroup, NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::{AppState, Runtime, Storage, router};

const SECRET: &[u8; 32] = b"thirty-two-byte-development-key!";

struct Harness {
    addr: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn url(h: &Harness, path: &str) -> String {
    format!("http://{}{}", h.addr, path)
}

fn no_redirect_client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client")
}

/// Start the host with two local users: an admin (`admin`) and a
/// regular user (`alice`). Seeds `groups` and `routes` into the
/// in-memory registry first, then returns a harness plus the user
/// ULIDs so tests can refer back to the seeded ownership.
async fn start_seeded(
    groups: &[(&str, &str)],
    routes: &[(&str, &str, &str)],
) -> (Harness, String, String) {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    // Create users first so we have ULIDs to seed groups/routes with.
    let admin = auth.create_user("admin", true).expect("create admin user");
    let alice = auth.create_user("alice", false).expect("create alice user");

    let registry = Arc::new(Registry::new(storage.clone()));

    // Seed groups. Each tuple is (name, owner_username); ownership
    // by username keeps the test inputs readable.
    for (name, owner_username) in groups {
        let owner_id = match *owner_username {
            "admin" => admin.id.clone(),
            "alice" => alice.id.clone(),
            other => panic!("unknown owner in test seed: {other}"),
        };
        registry
            .create_group(NewGroup {
                name: (*name).to_string(),
                owner_id,
                ttl_seconds: Some(3600),
                sliding_ttl: Some(true),
            })
            .expect("create group");
    }

    // Seed routes. Each tuple is (group_name, path, owner_username).
    // Empty wasm bytes are fine — the table never tries to execute
    // these because tests don't dispatch real requests through them.
    for (group_name, path, owner_username) in routes {
        let owner_id = match *owner_username {
            "admin" => admin.id.clone(),
            "alice" => alice.id.clone(),
            other => panic!("unknown owner: {other}"),
        };
        registry
            .create_route(NewRoute {
                group: Some((*group_name).to_string()),
                methods: vec!["GET".into()],
                path: (*path).to_string(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: vec![],
                owner_id,
                source: None,
            })
            .expect("create route");
    }

    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage.clone());
    let state = AppState::new(runtime, routes, auth, journal)
        .with_local_auth(
            LocalAuth::parse("admin:devpassword:admin,alice:devpassword").expect("local auth"),
        )
        .with_sessions(SessionStore::new(storage, SECRET).expect("sessions"));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (Harness { addr, server }, admin.id.clone(), alice.id.clone())
}

async fn login_cookie(h: &Harness, client: &Client, user: &str) -> String {
    // Slice-25 CSRF: GET login page first to mint cookie + read form
    // value, then POST with both.
    let get = client.get(url(h, "/auth/login")).send().await.unwrap();
    let csrf_cookie = pick_set_cookie(&get, "wm_csrf").expect("csrf cookie");
    let body = get.text().await.unwrap();
    let csrf_value = extract_csrf_value(&body).expect("csrf form value");

    let resp = client
        .post(url(h, "/auth/login/password"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("wm_csrf={csrf_cookie}"))
        .body(format!(
            "_csrf={csrf_value}&username={user}&password=devpassword"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 303, "login {user}");
    let session_cookie = pick_set_cookie(&resp, "wm_session").expect("session cookie");
    format!("wm_csrf={csrf_cookie}; wm_session={session_cookie}")
}

fn pick_set_cookie(resp: &reqwest::Response, name: &str) -> Option<String> {
    for v in resp.headers().get_all("set-cookie").iter() {
        let raw = v.to_str().ok()?;
        if let Some(rest) = raw.strip_prefix(&format!("{name}=")) {
            return Some(rest.split(';').next()?.to_string());
        }
    }
    None
}

fn extract_csrf_value(body: &str) -> Option<String> {
    let needle = "name=\"_csrf\" value=\"";
    let start = body.find(needle)? + needle.len();
    let end = body[start..].find('"')?;
    Some(body[start..start + end].to_string())
}

// -- /ui/groups -----------------------------------------------------------

#[tokio::test]
async fn groups_list_renders_all_groups_for_admin() {
    let (h, _admin_id, _alice_id) = start_seeded(
        &[("alpha", "admin"), ("beta", "alice"), ("gamma", "admin")],
        &[],
    )
    .await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/ui/groups"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Admin defaults to "everyone" — should see all three groups.
    assert!(body.contains("alpha"), "alpha visible: {body}");
    assert!(body.contains("beta"));
    assert!(body.contains("gamma"));
    assert!(body.contains("All groups on this host"));
    // Owner column should resolve usernames.
    assert!(body.contains("admin"));
    assert!(body.contains("alice"));
}

#[tokio::test]
async fn groups_list_scopes_non_admin_to_own_groups() {
    let (h, _admin_id, _alice_id) = start_seeded(
        &[("alpha", "admin"), ("beta", "alice"), ("gamma", "admin")],
        &[],
    )
    .await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;

    let body = client
        .get(url(&h, "/ui/groups"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("beta"), "alice's group visible");
    assert!(
        !body.contains(">alpha<"),
        "alice should not see admin's group: {body}"
    );
    assert!(!body.contains(">gamma<"));
    // Non-admin doesn't get the owner-scope filter at all.
    assert!(
        !body.contains("name=\"owner_scope\""),
        "non-admin shouldn't see owner-scope dropdown"
    );
}

#[tokio::test]
async fn groups_list_admin_can_filter_to_just_mine() {
    let (h, _admin_id, _alice_id) =
        start_seeded(&[("alpha", "admin"), ("beta", "alice")], &[]).await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/ui/groups?owner_scope=mine"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("alpha"));
    assert!(!body.contains(">beta<"));
}

#[tokio::test]
async fn groups_list_name_search_echoes_back_in_form() {
    let (h, _admin_id, _alice_id) =
        start_seeded(&[("alpha-prod", "admin"), ("beta-dev", "admin")], &[]).await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/ui/groups?q=alpha"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("alpha-prod"));
    assert!(!body.contains("beta-dev"));
    // Search input echoes the filter so the user can refine it.
    assert!(
        body.contains("value=\"alpha\""),
        "search input echoes the query"
    );
    // Reset link appears when any filter is active.
    assert!(body.contains("Reset"));
}

#[tokio::test]
async fn groups_list_sort_link_toggles_direction() {
    let (h, _admin_id, _alice_id) = start_seeded(&[("g1", "admin")], &[]).await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    // Default sort is last_activity_at desc; clicking the "name" sort
    // link should produce `?sort=name&dir=asc` (name's default
    // direction). Calling again with `?sort=name&dir=asc` should give
    // a header link to `?sort=name&dir=desc` for the next click.
    let body = client
        .get(url(&h, "/ui/groups?sort=name&dir=asc"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("sort=name") && body.contains("dir=desc"),
        "sort header should flip dir to desc when already asc: {body}"
    );
    // Arrow indicator shows current sort direction.
    assert!(body.contains("↑"), "asc arrow visible when sort=name asc");
}

#[tokio::test]
async fn groups_list_paginates_with_next_link() {
    // Seed 30 groups so we cross the 25/page boundary.
    let groups: Vec<(String, &str)> = (0..30).map(|i| (format!("g{i:02}"), "admin")).collect();
    let groups_ref: Vec<(&str, &str)> = groups.iter().map(|(n, o)| (n.as_str(), *o)).collect();
    let (h, _admin_id, _alice_id) = start_seeded(&groups_ref, &[]).await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/ui/groups"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Page 1 of 2"), "paged footer: {body}");
    assert!(
        body.contains("offset=25"),
        "next-page link with offset=25 present"
    );
    assert!(body.contains("30 groups"));
}

// -- /ui/routes -----------------------------------------------------------

#[tokio::test]
async fn routes_list_renders_routes_with_filter() {
    let (h, _admin_id, _alice_id) = start_seeded(
        &[("g1", "admin"), ("g2", "admin")],
        &[
            ("g1", "/users", "admin"),
            ("g1", "/orders", "admin"),
            ("g2", "/products", "admin"),
        ],
    )
    .await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let body = client
        .get(url(&h, "/ui/routes"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // minijinja auto-escapes `/` in element bodies; match on the
    // alphanumeric segment that survives the escape.
    assert!(body.contains("users"));
    assert!(body.contains("orders"));
    assert!(body.contains("products"));
    assert!(body.contains("3 routes"));

    // Group filter narrows the list.
    let body = client
        .get(url(&h, "/ui/routes?group=g1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("users"));
    assert!(body.contains("orders"));
    assert!(!body.contains("products"));
    assert!(body.contains("value=\"g1\""), "group filter echoes back");
}

#[tokio::test]
async fn routes_list_scopes_non_admin_to_own_routes() {
    let (h, _admin_id, _alice_id) = start_seeded(
        &[("g1", "admin"), ("g2", "alice")],
        &[
            ("g1", "/admin-route", "admin"),
            ("g2", "/alice-route", "alice"),
        ],
    )
    .await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;

    let body = client
        .get(url(&h, "/ui/routes"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Path slashes are HTML-escaped in the rendered body; match on the
    // alphanumeric label instead. The "admin-route" string would still
    // appear here if alice could see it.
    assert!(body.contains("alice-route"));
    assert!(!body.contains("admin-route"));
}

// -- Error path ------------------------------------------------------------

#[tokio::test]
async fn bad_sort_renders_400_placeholder() {
    let (h, _admin_id, _alice_id) = start_seeded(&[("g1", "admin")], &[]).await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "admin").await;

    let resp = client
        .get(url(&h, "/ui/groups?sort=bogus"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("filter error"));
}

#[tokio::test]
async fn non_admin_cannot_pass_owner_id_via_unknown_field() {
    // Sneaking owner_id into the URL is blocked at the core fn — the
    // UI hands it Option<owner_id> built from owner_scope only, so
    // even a malicious query string can't get past it. The UI input
    // struct doesn't have an `owner_id` field, so the raw param is
    // simply ignored by serde and the user gets their own groups.
    let (h, admin_id, _alice_id) =
        start_seeded(&[("alpha", "admin"), ("beta", "alice")], &[]).await;
    let client = no_redirect_client();
    let cookie = login_cookie(&h, &client, "alice").await;

    let body = client
        .get(url(&h, &format!("/ui/groups?owner_id={admin_id}")))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Alice still sees only her own groups; admin's "alpha" is hidden.
    assert!(body.contains("beta"));
    assert!(!body.contains(">alpha<"));
}
