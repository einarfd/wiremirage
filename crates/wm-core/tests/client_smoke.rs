//! Tier-1 client tests against an in-process axum mock that returns
//! canned responses. We assert request shape (path, headers,
//! User-Agent, Authorization, body) and response decoding into typed
//! structs. Doesn't talk to a real wm-host.

use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::json;
use wm_core::{
    Client, ClientError, CreateGroupBody, CreateRouteBody, CreateTokenBody, PatchGroupBody,
};

/// Captured request the mock saw on its way through. Tests inspect
/// this to verify what the client sent.
#[derive(Debug, Clone, Default)]
struct Captured {
    method: String,
    path: String,
    user_agent: Option<String>,
    authorization: Option<String>,
    body: Option<serde_json::Value>,
}

#[derive(Default)]
struct MockState {
    last: Mutex<Option<Captured>>,
}

async fn capture_get(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> axum::Json<serde_json::Value> {
    *state.last.lock().unwrap() = Some(captured_no_body("GET", "/__health", &headers));
    axum::Json(json!({ "status": "ok", "version": "0.0.0" }))
}

async fn capture_groups_list(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> axum::Json<serde_json::Value> {
    *state.last.lock().unwrap() = Some(captured_no_body("GET", "/__api/groups", &headers));
    axum::Json(json!({
        "groups": [
            {
                "id": "01HZG",
                "name": "stripe-mock",
                "implicit": false,
                "owner_id": "01HOWNER",
                "ttl_seconds": 86400,
                "sliding_ttl": true,
                "created_at": "2026-05-01T18:00:00+00:00"
            }
        ]
    }))
}

async fn capture_groups_create(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    *state.last.lock().unwrap() = Some(captured(
        "POST",
        "/__api/groups",
        &headers,
        Some(body.clone()),
    ));
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "id": "01HZG",
            "name": body.get("name").cloned().unwrap_or(json!("anon")),
            "implicit": false,
            "owner_id": "01HOWNER",
            "ttl_seconds": body.get("ttl_seconds").and_then(|v| v.as_u64()).unwrap_or(86400),
            "sliding_ttl": body.get("sliding_ttl").and_then(|v| v.as_bool()).unwrap_or(true),
            "created_at": "2026-05-01T18:00:00+00:00"
        })),
    )
}

async fn capture_groups_show(
    State(state): State<Arc<MockState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    *state.last.lock().unwrap() = Some(captured_no_body(
        "GET",
        &format!("/__api/groups/{name}"),
        &headers,
    ));
    if name == "missing" {
        return Err(StatusCode::NOT_FOUND);
    }
    if name == "secret" {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(axum::Json(json!({
        "id": "01HZG",
        "name": name,
        "implicit": false,
        "owner_id": "01HOWNER",
        "ttl_seconds": 86400,
        "sliding_ttl": true,
        "created_at": "2026-05-01T18:00:00+00:00"
    })))
}

async fn capture_groups_patch(
    State(state): State<Arc<MockState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *state.last.lock().unwrap() = Some(captured(
        "PATCH",
        &format!("/__api/groups/{name}"),
        &headers,
        Some(body.clone()),
    ));
    axum::Json(json!({
        "id": "01HZG",
        "name": name,
        "implicit": false,
        "owner_id": "01HOWNER",
        "ttl_seconds": body.get("ttl_seconds").and_then(|v| v.as_u64()).unwrap_or(86400),
        "sliding_ttl": body.get("sliding_ttl").and_then(|v| v.as_bool()).unwrap_or(true),
        "created_at": "2026-05-01T18:00:00+00:00"
    }))
}

async fn capture_groups_delete(
    State(state): State<Arc<MockState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    *state.last.lock().unwrap() = Some(captured_no_body(
        "DELETE",
        &format!("/__api/groups/{name}"),
        &headers,
    ));
    StatusCode::NO_CONTENT
}

async fn capture_tokens_create(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    *state.last.lock().unwrap() = Some(captured(
        "POST",
        "/__api/tokens",
        &headers,
        Some(body.clone()),
    ));
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "token": "wmt_plaintext_only_visible_here",
            "record": {
                "id": "01HTOK",
                "name": body.get("name").cloned().unwrap_or(json!("anon")),
                "owner_id": "01HOWNER",
                "created_at": "2026-05-01T18:00:00+00:00",
                "expires_at": null,
                "last_used_at": null,
                "scopes": ["*"]
            }
        })),
    )
}

#[derive(Debug, Deserialize)]
struct JournalQuery {
    before: Option<u32>,
    limit: Option<usize>,
}

async fn capture_journal_list(
    State(state): State<Arc<MockState>>,
    Path(group): Path<String>,
    Query(q): Query<JournalQuery>,
    headers: HeaderMap,
) -> axum::Json<serde_json::Value> {
    let mut path = format!("/__api/journal/{group}");
    let mut params: Vec<String> = Vec::new();
    if let Some(b) = q.before {
        params.push(format!("before={b}"));
    }
    if let Some(l) = q.limit {
        params.push(format!("limit={l}"));
    }
    if !params.is_empty() {
        path.push('?');
        path.push_str(&params.join("&"));
    }
    *state.last.lock().unwrap() = Some(captured_no_body("GET", &path, &headers));
    axum::Json(json!({
        "entries": [],
        "next_before": null
    }))
}

fn captured(
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: Option<serde_json::Value>,
) -> Captured {
    Captured {
        method: method.into(),
        path: path.into(),
        user_agent: header_str(headers, "user-agent"),
        authorization: header_str(headers, "authorization"),
        body,
    }
}

fn captured_no_body(method: &str, path: &str, headers: &HeaderMap) -> Captured {
    captured(method, path, headers, None)
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

async fn start_mock() -> (Arc<MockState>, String, tokio::task::JoinHandle<()>) {
    let state = Arc::new(MockState::default());
    let app = Router::new()
        .route("/__health", get(capture_get))
        .route(
            "/__api/groups",
            get(capture_groups_list).post(capture_groups_create),
        )
        .route(
            "/__api/groups/{name}",
            get(capture_groups_show)
                .patch(capture_groups_patch)
                .delete(capture_groups_delete),
        )
        .route("/__api/tokens", post(capture_tokens_create))
        .route("/__api/journal/{group}", get(capture_journal_list))
        // Two unused routes so we can verify the right verb fires:
        .route("/__api/groups/{name}/refresh", post(|| async {}))
        .route("/__api/groups/{name}/state", delete(|| async {}))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (state, format!("http://{addr}"), server)
}

#[tokio::test]
async fn health_round_trips() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let h = client.health().await.expect("health");
    assert_eq!(h.status, "ok");
    assert_eq!(h.version, "0.0.0");
    let cap = state.last.lock().unwrap().clone().unwrap();
    assert_eq!(cap.method, "GET");
    assert_eq!(cap.path, "/__health");
    // User-Agent default is wm-cli/{CARGO_PKG_VERSION-of-wm-core}.
    let ua = cap.user_agent.expect("user-agent");
    assert!(ua.starts_with("wm-cli/"));
    server.abort();
}

#[tokio::test]
async fn user_agent_can_be_overridden() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host)
        .with_user_agent("wm-mcp/0.0.0")
        .build()
        .expect("build");
    let _ = client.health().await.expect("health");
    let cap = state.last.lock().unwrap().clone().unwrap();
    assert_eq!(cap.user_agent.as_deref(), Some("wm-mcp/0.0.0"));
    server.abort();
}

#[tokio::test]
async fn token_is_sent_as_bearer_authorization() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host)
        .with_token("wmt_test_token")
        .build()
        .expect("build");
    client.list_groups().await.expect("list");
    let cap = state.last.lock().unwrap().clone().unwrap();
    assert_eq!(cap.authorization.as_deref(), Some("Bearer wmt_test_token"));
    server.abort();
}

#[tokio::test]
async fn no_token_means_no_authorization_header() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let _ = client.health().await.expect("health");
    let cap = state.last.lock().unwrap().clone().unwrap();
    assert!(cap.authorization.is_none());
    server.abort();
}

#[tokio::test]
async fn create_group_serializes_body_and_decodes_response() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let body = CreateGroupBody {
        name: "stripe-mock".into(),
        ttl_seconds: Some(3600),
        sliding_ttl: Some(false),
    };
    let g = client.create_group(&body).await.expect("create");
    assert_eq!(g.name, "stripe-mock");
    assert_eq!(g.ttl_seconds, 3600);
    assert!(!g.sliding_ttl);

    let cap = state.last.lock().unwrap().clone().unwrap();
    assert_eq!(cap.method, "POST");
    let sent = cap.body.unwrap();
    assert_eq!(sent["name"], "stripe-mock");
    assert_eq!(sent["ttl_seconds"], 3600);
    assert_eq!(sent["sliding_ttl"], false);
    server.abort();
}

#[tokio::test]
async fn patch_group_skips_none_fields() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let body = PatchGroupBody {
        ttl_seconds: Some(7200),
        ..Default::default()
    };
    client
        .patch_group("stripe-mock", &body)
        .await
        .expect("patch");
    let cap = state.last.lock().unwrap().clone().unwrap();
    let sent = cap.body.unwrap();
    assert_eq!(sent["ttl_seconds"], 7200);
    assert!(
        sent.get("sliding_ttl").is_none(),
        "None fields should be omitted from the JSON body"
    );
    server.abort();
}

#[tokio::test]
async fn delete_returns_unit_on_204() {
    let (_state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    client.delete_group("stripe-mock").await.expect("delete");
    server.abort();
}

#[tokio::test]
async fn not_found_translates_to_typed_error() {
    let (_state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let err = client.get_group("missing").await.unwrap_err();
    assert!(matches!(err, ClientError::NotFound(_)));
    server.abort();
}

#[tokio::test]
async fn forbidden_translates_to_typed_error() {
    let (_state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let err = client.get_group("secret").await.unwrap_err();
    assert!(matches!(err, ClientError::Forbidden(_)));
    server.abort();
}

#[tokio::test]
async fn create_token_returns_plaintext_in_response() {
    let (_state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    let body = CreateTokenBody {
        name: "ci-runner".into(),
        ttl_seconds: None,
    };
    let resp = client.create_token(&body).await.expect("create");
    assert!(resp.token.starts_with("wmt_"));
    assert_eq!(resp.record.name, "ci-runner");
    server.abort();
}

#[tokio::test]
async fn journal_list_serializes_pagination_query_params() {
    let (state, host, server) = start_mock().await;
    let client = Client::builder(host).build().expect("build");
    client
        .list_journal("stripe-mock", Some(7), Some(20))
        .await
        .expect("list");
    let cap = state.last.lock().unwrap().clone().unwrap();
    assert_eq!(cap.path, "/__api/journal/stripe-mock?before=7&limit=20");
    server.abort();
}

#[tokio::test]
async fn route_create_body_omits_unset_fields() {
    let (_state, host, server) = start_mock().await;
    let _client = Client::builder(host).build().expect("build");
    // We don't have a route mock here; just verify the body
    // serializes correctly via a direct serde check: an unset optional
    // field (group) is omitted, and source is carried through.
    let body = CreateRouteBody {
        group: None,
        methods: vec!["POST".into()],
        path: "/v1/charges".into(),
        language: "javascript".into(),
        source: Some("function handle() {}".into()),
    };
    let json = serde_json::to_value(&body).unwrap();
    assert!(json.get("group").is_none(), "unset group omitted");
    assert_eq!(json["language"], "javascript");
    assert_eq!(json["source"], "function handle() {}");
    server.abort();
}
