//! Tier-2 SSE tests for `GET /__api/journal/tail`. Spins up a live
//! wm-host on a random port, opens the SSE endpoint as a reqwest
//! streaming response, drives writes via `Journal::record_handled` /
//! `record_unmatched` directly (cheaper than firing real mock
//! traffic), and asserts events arrive with the expected shape.

use std::sync::Arc;
use std::time::Duration;

use wm_host::auth::Auth;
use wm_host::journal::{
    HandlerLogEntry, Journal, NewJournalEntry, NewUnmatchedEntry, RequestEnvelope, ResourceUsage,
    ResponseEnvelope,
};
use wm_host::registry::{NewGroup, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const ADMIN_TOKEN: &str = "wmt_test_admin";

struct Harness {
    base_url: String,
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
    auth.bootstrap_admin("admin", ADMIN_TOKEN)
        .expect("bootstrap");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        base_url: format!("http://{addr}"),
        state,
        server,
    }
}

fn sample_handled(group_id: &str, group_name: &str, method: &str, status: u16) -> NewJournalEntry {
    NewJournalEntry {
        trace_id: None,
        group_id: group_id.into(),
        group_name: group_name.into(),
        route_id: "r1".into(),
        route_number: 1,
        matched_pattern: "/v1/charges".into(),
        request: RequestEnvelope {
            method: method.into(),
            path: "/v1/charges".into(),
            headers: vec![],
            body: vec![],
            original_body_size: 0,
            body_truncated: false,
        },
        response: ResponseEnvelope {
            status,
            headers: vec![],
            body: vec![],
            original_body_size: 0,
            body_truncated: false,
        },
        path_params: vec![],
        query: vec![],
        handler_logs: Vec::<HandlerLogEntry>::new(),
        duration_ms: 5,
        resources: ResourceUsage::default(),
        error: None,
        dropped_response_headers: vec![],
    }
}

/// Open the SSE endpoint and read raw bytes. We don't spin up a full
/// SSE parser — for our assertions, scanning the text stream for
/// `event:` and `data:` lines is enough.
async fn open_tail(base_url: &str, token: &str, query: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base_url}/__api/journal/tail{query}"))
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("send")
}

/// Read one event from the SSE stream, where "event" is everything
/// up to a blank line. Returns the joined raw text. Times out after
/// `timeout` so a stuck test fails fast.
async fn read_one_event(body: &mut reqwest::Response, timeout: Duration) -> Option<String> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, body.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                // Find a blank-line terminator.
                if let Some(idx) = buf.find("\n\n") {
                    let event = buf[..idx].to_string();
                    // Skip keep-alive comments — just continue reading.
                    if event.lines().all(|l| l.is_empty() || l.starts_with(':')) {
                        buf = buf[idx + 2..].to_string();
                        continue;
                    }
                    return Some(event);
                }
            }
            Ok(Ok(None)) => return None,
            Ok(Err(_)) | Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn tail_requires_auth() {
    let h = start().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/__api/journal/tail", h.base_url))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn tail_host_wide_is_admin_only() {
    let h = start().await;
    // Provision a non-admin user with their own token.
    let user = h
        .state
        .auth()
        .create_user("alice", false)
        .expect("create user");
    let (_token, user_plaintext) = h
        .state
        .auth()
        .create_token(&user.id, "default", None)
        .expect("create token");

    let resp = open_tail(&h.base_url, &user_plaintext, "").await;
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn tail_emits_handled_events_to_admin() {
    let h = start().await;
    // Subscribe BEFORE writing so we don't miss the event.
    let mut resp = open_tail(&h.base_url, ADMIN_TOKEN, "").await;
    assert_eq!(resp.status().as_u16(), 200);

    // Drive a journal write directly.
    let entry = sample_handled("g1", "stripe-mock", "POST", 200);
    h.state
        .journal()
        .record_handled(entry)
        .expect("record_handled");

    let event = read_one_event(&mut resp, Duration::from_secs(2))
        .await
        .expect("got an event");
    assert!(event.contains("event:handled") || event.contains("event: handled"));
    assert!(event.contains("\"group_name\":\"stripe-mock\""));
    assert!(event.contains("\"matched_pattern\":\"/v1/charges\""));
}

#[tokio::test]
async fn tail_filters_by_group() {
    let h = start().await;
    // Need a real group in storage so the auth gate can resolve it.
    h.state
        .routes()
        .registry()
        .create_group(NewGroup {
            name: "stripe-mock".into(),
            owner_id: "admin-id".into(),
            ttl_seconds: None,
            sliding_ttl: None,
        })
        .expect("create group");
    let stripe = h
        .state
        .routes()
        .registry()
        .read_group_by_ref("stripe-mock")
        .unwrap();

    let mut resp = open_tail(&h.base_url, ADMIN_TOKEN, "?group=stripe-mock").await;
    assert_eq!(resp.status().as_u16(), 200);

    // Write to a different group (twilio) — should not arrive.
    let other = sample_handled("twilio-id", "twilio-mock", "POST", 200);
    h.state.journal().record_handled(other).unwrap();

    // Then write to the matching group.
    let stripe_entry = sample_handled(&stripe.id, &stripe.name, "POST", 200);
    h.state.journal().record_handled(stripe_entry).unwrap();

    let event = read_one_event(&mut resp, Duration::from_secs(2))
        .await
        .expect("got an event");
    assert!(event.contains("\"group_name\":\"stripe-mock\""));
    assert!(!event.contains("twilio-mock"));
}

#[tokio::test]
async fn tail_filters_by_method() {
    let h = start().await;
    let mut resp = open_tail(&h.base_url, ADMIN_TOKEN, "?method=POST").await;
    assert_eq!(resp.status().as_u16(), 200);

    // GET should be filtered out.
    h.state
        .journal()
        .record_handled(sample_handled("g1", "g1", "GET", 200))
        .unwrap();
    h.state
        .journal()
        .record_handled(sample_handled("g1", "g1", "POST", 201))
        .unwrap();

    let event = read_one_event(&mut resp, Duration::from_secs(2))
        .await
        .expect("got an event");
    assert!(event.contains("\"status\":201"));
    assert!(!event.contains("\"status\":200")); // the GET was filtered before reaching us
}

#[tokio::test]
async fn tail_emits_unmatched_when_no_filters() {
    let h = start().await;
    let mut resp = open_tail(&h.base_url, ADMIN_TOKEN, "").await;
    assert_eq!(resp.status().as_u16(), 200);

    h.state
        .journal()
        .record_unmatched(NewUnmatchedEntry {
            trace_id: None,
            request: RequestEnvelope {
                method: "GET".into(),
                path: "/missing".into(),
                headers: vec![],
                body: vec![],
                original_body_size: 0,
                body_truncated: false,
            },
        })
        .expect("record_unmatched");

    let event = read_one_event(&mut resp, Duration::from_secs(2))
        .await
        .expect("got an event");
    assert!(event.contains("event:unmatched") || event.contains("event: unmatched"));
    assert!(event.contains("\"path\":\"/missing\""));
}

#[tokio::test]
async fn tail_rejects_invalid_filter() {
    let h = start().await;
    let resp = open_tail(&h.base_url, ADMIN_TOKEN, "?route=not-a-slug").await;
    assert_eq!(resp.status().as_u16(), 400);
}
