//! Tier-2 end-to-end tests for outbound callbacks (ADR-0034 slice 3).
//!
//! Boots an engine-backed host plus a tiny in-process "system under test"
//! HTTP server, registers a TS handler that calls `host.scheduleCallback`,
//! and checks the gate (host egress + per-group opt-in), real delivery, and
//! the egress filter (loopback denied unless allow-listed). All paths land an
//! outcome in the per-group callback journal.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Client;
use serde_json::json;
use wm_host::auth::Auth;
use wm_host::egress::EgressPolicy;
use wm_host::journal::{CallbackOutcome, Journal, ListCursor};
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_callbacks";

fn js_engine_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

/// A TS handler that schedules one callback to `sut_url` and reports, in its
/// own response body, whether scheduling threw (so the gate is observable from
/// the dispatch response, not just the journal).
fn callback_source(sut_url: &str) -> String {
    format!(
        r#"function handle(req, route, group) {{
            try {{
                host.scheduleCallback({{
                    url: "{sut_url}",
                    method: "POST",
                    headers: [["content-type", "application/json"]],
                    body: JSON.stringify({{ event: "ping" }}),
                    delayMs: 0,
                }});
                return {{ status: 200, headers: [], body: new TextEncoder().encode("scheduled") }};
            }} catch (e) {{
                return {{ status: 200, headers: [], body: new TextEncoder().encode("ERR:" + e.message) }};
            }}
        }}"#
    )
}

struct Harness {
    addr: String,
    client: Client,
    state: AppState,
    server: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// Build an engine-backed host with the given egress policy. Returns
    /// `None` if the build-time js-engine artifact is missing (so the test
    /// skips rather than failing in a broken build).
    async fn start(egress: EgressPolicy) -> Option<Self> {
        let p = js_engine_path()?;
        let storage = Storage::in_memory();
        let auth = Auth::new(storage.clone());
        auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
            .expect("bootstrap admin");
        let runtime = Arc::new(
            Runtime::new(storage.clone())
                .expect("runtime")
                .with_js_engine(&p)
                .expect("attach js engine"),
        );
        let registry = Arc::new(Registry::new(storage.clone()));
        let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
        let journal = Journal::new(storage);
        let state = AppState::new(runtime, routes, auth, journal).with_egress(egress);
        let app = router(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {BOOTSTRAP_TOKEN}")).unwrap(),
        );
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .expect("client");
        Some(Harness {
            addr,
            client,
            state,
            server,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Create a group (named) and a callback-scheduling route in it, with the
    /// group's `callout_enabled` set to `opt_in`. Returns `(group_id,
    /// group_name)`.
    async fn setup_group_and_route(
        &self,
        name: &str,
        opt_in: bool,
        sut_url: &str,
    ) -> (String, String) {
        let g: serde_json::Value = self
            .client
            .post(self.url("/api/groups"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("create group")
            .json()
            .await
            .expect("group json");
        let group_id = g["id"].as_str().expect("group id").to_string();
        let group_name = g["name"].as_str().expect("group name").to_string();

        if opt_in {
            let resp = self
                .client
                .patch(self.url(&format!("/api/groups/{group_name}")))
                .json(&json!({ "callout_enabled": true }))
                .send()
                .await
                .expect("patch callout");
            assert_eq!(resp.status().as_u16(), 200);
        }

        let resp = self
            .client
            .post(self.url("/api/routes"))
            .json(&json!({
                "group": group_name,
                "methods": ["POST"],
                "path": "/charge",
                "language": "javascript",
                "source": callback_source(sut_url),
            }))
            .send()
            .await
            .expect("create route");
        assert_eq!(resp.status().as_u16(), 201, "route create failed");
        (group_id, group_name)
    }

    /// Drive a mock request at the route (virtual-host addressed) and return
    /// the response body text.
    async fn dispatch(&self, group_name: &str) -> String {
        let resp = self
            .client
            .post(self.url("/charge"))
            .header(reqwest::header::HOST, format!("{group_name}.localhost"))
            .send()
            .await
            .expect("dispatch");
        resp.text().await.expect("body")
    }

    /// Poll the callback journal for `group_id` until at least one record
    /// appears or the timeout elapses.
    async fn wait_for_callback(&self, group_id: &str) -> Option<wm_host::journal::CallbackRecord> {
        for _ in 0..50 {
            let recs = self
                .state
                .journal()
                .list_callbacks_for_group(group_id, ListCursor::default())
                .expect("list callbacks");
            if let Some(r) = recs.into_iter().next() {
                return Some(r);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A throwaway "system under test" HTTP server that records the bodies it
/// receives on `POST /hook`. Returns its callback URL, the shared sink, and
/// the server task handle.
async fn spawn_sut() -> (
    String,
    Arc<Mutex<Vec<Vec<u8>>>>,
    tokio::task::JoinHandle<()>,
) {
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |body: axum::body::Bytes| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(body.to_vec());
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sut");
    let port = listener.local_addr().expect("sut addr").port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve sut");
    });
    (format!("http://127.0.0.1:{port}/hook"), received, handle)
}

#[tokio::test]
async fn callback_rejected_when_host_egress_disabled() {
    // Default posture: egress off → scheduleCallback throws, the handler
    // catches it, and nothing is fired.
    let Some(h) = Harness::start(EgressPolicy::disabled()).await else {
        eprintln!("skip: js-engine artifact missing");
        return;
    };
    let (group_id, group_name) = h
        .setup_group_and_route("noegress", true, "http://127.0.0.1:1/hook")
        .await;
    let body = h.dispatch(&group_name).await;
    assert!(
        body.starts_with("ERR:"),
        "expected a thrown rejection, got: {body}"
    );
    assert!(
        body.contains("not enabled"),
        "rejection should explain: {body}"
    );
    // Nothing scheduled → no callback journal entry.
    let recs = h
        .state
        .journal()
        .list_callbacks_for_group(&group_id, ListCursor::default())
        .expect("list");
    assert!(recs.is_empty());
}

#[tokio::test]
async fn callback_rejected_when_group_not_opted_in() {
    // Egress on host-wide, but the group didn't set callout_enabled → still
    // rejected. Per-group opt-in is required on top of the host capability.
    let egress = EgressPolicy::new(true, vec!["127.0.0.0/8".parse().unwrap()], vec![]);
    let Some(h) = Harness::start(egress).await else {
        eprintln!("skip: js-engine artifact missing");
        return;
    };
    let (_group_id, group_name) = h
        .setup_group_and_route("optout", false, "http://127.0.0.1:1/hook")
        .await;
    let body = h.dispatch(&group_name).await;
    assert!(body.starts_with("ERR:"), "expected rejection, got: {body}");
    assert!(
        body.contains("callout_enabled"),
        "rejection should name the flag: {body}"
    );
}

#[tokio::test]
async fn callback_delivered_to_sut_and_journaled() {
    // Egress on + 127/8 allow-listed + group opted in → the callback fires to
    // the local SUT and the outcome is journaled as delivered.
    let (sut_url, received, _sut) = spawn_sut().await;
    let egress = EgressPolicy::new(true, vec!["127.0.0.0/8".parse().unwrap()], vec![]);
    let Some(h) = Harness::start(egress).await else {
        eprintln!("skip: js-engine artifact missing");
        return;
    };
    let (group_id, group_name) = h.setup_group_and_route("deliver", true, &sut_url).await;

    let body = h.dispatch(&group_name).await;
    assert_eq!(body, "scheduled", "handler should not have thrown: {body}");

    let rec = h
        .wait_for_callback(&group_id)
        .await
        .expect("a callback record should be journaled");
    match rec.outcome {
        CallbackOutcome::Delivered { status } => assert_eq!(status, 200),
        other => panic!("expected delivered, got {other:?}"),
    }
    assert_eq!(rec.method, "POST");
    assert_eq!(rec.url, sut_url);

    // The SUT actually received the webhook body.
    let got = received.lock().unwrap().clone();
    assert_eq!(
        got.len(),
        1,
        "SUT should have received exactly one callback"
    );
    assert_eq!(got[0], br#"{"event":"ping"}"#);
}

#[tokio::test]
async fn callback_to_loopback_denied_by_default() {
    // Egress on but NO allow-list → loopback is special-use and denied. The
    // group is opted in and the handler doesn't throw (the per-IP decision is
    // at fire time), but the journal records egress_denied and the SUT gets
    // nothing.
    let (sut_url, received, _sut) = spawn_sut().await;
    let Some(h) = Harness::start(EgressPolicy::new(true, vec![], vec![])).await else {
        eprintln!("skip: js-engine artifact missing");
        return;
    };
    let (group_id, group_name) = h.setup_group_and_route("denied", true, &sut_url).await;

    let body = h.dispatch(&group_name).await;
    assert_eq!(body, "scheduled", "scheduling itself succeeds: {body}");

    let rec = h
        .wait_for_callback(&group_id)
        .await
        .expect("an egress-denied outcome should still be journaled");
    match rec.outcome {
        CallbackOutcome::EgressDenied { reason, resolved } => {
            assert!(!reason.is_empty());
            assert!(resolved.iter().any(|ip| ip.starts_with("127.")));
        }
        other => panic!("expected egress_denied, got {other:?}"),
    }
    // Give any erroneously-fired request a moment to land, then assert none did.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        received.lock().unwrap().is_empty(),
        "SUT must not be reached"
    );
}
