//! Response shapes returned by the host's REST API.
//!
//! These mirror the wire format documented in `rest-api.md`. The host
//! defines its own serializable types in `wm-host::api`; we
//! deliberately duplicate them here rather than share definitions
//! across crates, so the JSON contract is the contract and the two
//! sides only have to agree on field names + types — not on Rust
//! type identity. If duplication becomes painful we can extract a
//! shared types crate later; today the redundancy is small enough.
//!
//! Field naming is `snake_case` (matches the host's serde defaults).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReadyResponse {
    pub status: String,
    pub valkey: String,
    pub compiler: String,
    pub version: String,
}

// -- Groups ------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GroupRecord {
    pub id: String,
    pub name: String,
    pub implicit: bool,
    pub owner_id: String,
    pub ttl_seconds: u64,
    pub sliding_ttl: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListGroupsResponse {
    pub groups: Vec<GroupRecord>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateGroupBody {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliding_ttl: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PatchGroupBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sliding_ttl: Option<bool>,
}

// -- Routes ------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteRecord {
    pub id: String,
    pub number: u32,
    pub group: GroupRef,
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    pub bindings_version: String,
    pub created_at: String,
    pub owner_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GroupRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListRoutesResponse {
    pub routes: Vec<RouteRecord>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateRouteBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bindings_version: Option<String>,
    /// Base64-encoded component when `language == "wasm"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compiled_wasm: Option<String>,
    /// Source code when `language` is a compiler-supported language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// -- Journal -----------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JournalRecord {
    pub id: String,
    pub number: u32,
    pub trace_id: Option<String>,
    pub group_id: String,
    pub group_name: String,
    pub route_id: String,
    pub route_number: u32,
    pub matched_pattern: String,
    pub request: RequestEnvelope,
    pub response: ResponseEnvelope,
    pub path_params: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub handler_logs: Vec<HandlerLogEntry>,
    pub duration_ms: u64,
    pub resources: ResourceUsage,
    pub error: Option<String>,
    pub dropped_response_headers: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RequestEnvelope {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub original_body_size: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseEnvelope {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub original_body_size: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HandlerLogEntry {
    pub level: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ResourceUsage {
    pub fuel_consumed: u64,
    pub memory_peak_bytes: u64,
    pub wall_clock_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListJournalResponse {
    pub entries: Vec<JournalRecord>,
    pub next_before: Option<u32>,
}

// -- Tokens ------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateTokenResponse {
    /// Plaintext token. Present **only** in the create response; never
    /// retrievable later.
    pub token: String,
    pub record: TokenRecord,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListTokensResponse {
    pub tokens: Vec<TokenRecord>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateTokenBody {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

// -- Errors ------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}
