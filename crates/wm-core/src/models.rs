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
    /// Most recent matched dispatch against any route in the group.
    /// `None` for groups that have never seen traffic.
    #[serde(default)]
    pub last_activity_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListGroupsResponse {
    pub groups: Vec<GroupRecord>,
    /// Total matches after filters, before pagination. Pre-slice-18
    /// hosts omit this field; we default to 0 in that case.
    #[serde(default)]
    pub total: u64,
    /// Pass back as `?offset=` to fetch the next page; `None` when
    /// the returned page reached the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
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
    /// Rename the group (its name is also its subdomain). Omitted from the
    /// wire when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Full public URL the SUT calls: `{scheme}://{group}.{apex}{path}`
    /// (ADR-0030 virtual-host routing). `#[serde(default)]` keeps the client
    /// tolerant of responses minted before the field existed.
    #[serde(default)]
    pub url: Option<String>,
    pub language: String,
    pub bindings_version: String,
    pub created_at: String,
    pub owner_id: String,
    /// Cumulative count of matched dispatches against this route.
    /// `0` for never-hit routes.
    #[serde(default)]
    pub hits_total: u64,
    /// Most recent matched dispatch against this route. `None` for
    /// never-hit routes.
    #[serde(default)]
    pub last_hit_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GroupRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListRoutesResponse {
    pub routes: Vec<RouteRecord>,
    #[serde(default)]
    pub total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateRouteBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    /// Handler source. The only artifact input (ADR-0023); pair with
    /// `language: "typescript" | "javascript"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Partial-update payload for `PATCH /__api/routes/{group}/{n}`. Send
/// only the fields you want to change. `language` is required when
/// replacing the handler `source`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PatchRouteBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// -- Route state + dry-run ---------------------------------------------------

/// A byte value on the wire (ADR-0025 / ADR-0026): a bare UTF-8 string,
/// or `{ "base64": "<...>" }` for binary. Mirrors `wm_host::wire::WireBytes`.
/// Used for state entries and (via [`bytes_field`]) request/response bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireBytes {
    Text(String),
    Binary { base64: String },
}

impl WireBytes {
    /// Wrap raw bytes for sending: a string when valid UTF-8, else base64.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        use base64::Engine as _;
        match std::str::from_utf8(bytes) {
            Ok(s) => WireBytes::Text(s.to_owned()),
            Err(_) => WireBytes::Binary {
                base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        }
    }

    /// Decode to raw bytes (e.g. when reading a snapshot back).
    pub fn into_bytes(self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine as _;
        match self {
            WireBytes::Text(s) => Ok(s.into_bytes()),
            WireBytes::Binary { base64 } => {
                base64::engine::general_purpose::STANDARD.decode(base64)
            }
        }
    }
}

/// `#[serde(with = "crate::models::bytes_field")]` for a `Vec<u8>` field
/// whose JSON form is [`WireBytes`] (string-first) — request/response
/// bodies. Field stays `Vec<u8>`; only the wire encoding changes.
pub mod bytes_field {
    use super::WireBytes;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        WireBytes::from_bytes(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        WireBytes::deserialize(d)?
            .into_bytes()
            .map_err(serde::de::Error::custom)
    }
}

/// Write payload for `PUT /__api/{routes,groups}/.../state`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetStateBody {
    pub entries: std::collections::HashMap<String, WireBytes>,
}

/// Round-trippable response for `GET .../state?format=snapshot` — bytes
/// entries only, in the same shape `SetStateBody` accepts.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StateSnapshotResponse {
    pub entries: std::collections::HashMap<String, WireBytes>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteStateEntry {
    pub key: String,
    pub kind: String,
    /// Set when `kind == "bytes"`. Collection-typed values report
    /// their cardinality via `length` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListRouteStateResponse {
    pub entries: Vec<RouteStateEntry>,
}

/// Response shape for `GET /__api/routes/{group}/{n}/source`. `source`
/// is `None` for pre-compiled `wasm` uploads (no source ever existed)
/// and for records that pre-date the source-storage slice.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteSourceResponse {
    pub slug: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DryRunBody {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "crate::models::bytes_field"
    )]
    pub body: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_params: Option<Vec<(String, String)>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<(String, String)>,
    /// Pre-populate the route's private `kv:` snapshot with these
    /// entries before the handler runs. Lets agents exercise state-
    /// dependent branches without driving real traffic first. Values
    /// use the ADR-0025 [`WireBytes`] encoding. Real state is never
    /// touched — overrides land in the disposable dry-run namespace.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub kv_overrides: std::collections::HashMap<String, WireBytes>,
    /// Same as `kv_overrides`, scoped to the group's shared `gkv:`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub gkv_overrides: std::collections::HashMap<String, WireBytes>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DryRunResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(with = "crate::models::bytes_field")]
    pub body: Vec<u8>,
    pub handler_logs: Vec<DryRunLog>,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub snapshot_keys: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DryRunLog {
    pub level: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
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
    #[serde(with = "crate::models::bytes_field")]
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub original_body_size: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseEnvelope {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(with = "crate::models::bytes_field")]
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

// -- Unmatched ---------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UnmatchedRecord {
    pub id: String,
    pub number: u64,
    /// The group the request was addressed to (`{group}.{apex}`,
    /// ADR-0030). `#[serde(default)]` for tolerance of records minted
    /// before group attribution existed.
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub group_name: String,
    pub trace_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub request: RequestEnvelope,
    /// Routes that nearly matched. Populated by the dispatcher at
    /// unmatched-write time (slice 35); empty when nothing nearby
    /// was found. Mirror of the host's `UnmatchedNearMiss`.
    #[serde(default)]
    pub near_misses: Vec<UnmatchedNearMiss>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UnmatchedNearMiss {
    /// `{group}/{number}` slug.
    pub route: String,
    pub route_path: String,
    pub route_methods: Vec<String>,
    pub reason: UnmatchedNearMissReason,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnmatchedNearMissReason {
    MethodMismatch {
        expected_methods: Vec<String>,
        got: String,
    },
    PrefixMatch {
        segment_index: usize,
        expected: String,
        got: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListUnmatchedResponse {
    pub entries: Vec<UnmatchedRecord>,
    pub next_before: Option<u64>,
}

// -- List parameters ---------------------------------------------------------
//
// One struct per list endpoint, all `Default` + builder-friendly. The
// `to_query_string` helpers serialize to `key=value&...` with URL
// encoding. Empty params produce an empty string (no `?` prefix); the
// caller adds the prefix only when the result is non-empty.

#[derive(Debug, Clone, Default)]
pub struct ListGroupsParams {
    pub owner_id: Option<String>,
    pub name_prefix: Option<String>,
    pub q: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub implicit: Option<bool>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

impl ListGroupsParams {
    pub fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        push_opt(&mut parts, "owner_id", self.owner_id.as_deref());
        push_opt(&mut parts, "name_prefix", self.name_prefix.as_deref());
        push_opt(&mut parts, "q", self.q.as_deref());
        push_opt(&mut parts, "since", self.since.as_deref());
        push_opt(&mut parts, "until", self.until.as_deref());
        if let Some(b) = self.implicit {
            parts.push(format!("implicit={b}"));
        }
        push_opt(&mut parts, "sort", self.sort.as_deref());
        push_opt(&mut parts, "dir", self.dir.as_deref());
        push_opt_num(&mut parts, "offset", self.offset);
        push_opt_num(&mut parts, "limit", self.limit);
        parts.join("&")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListRoutesParams {
    pub group: Option<String>,
    pub owner_id: Option<String>,
    pub method: Option<String>,
    pub path_pattern: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub q: Option<String>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

impl ListRoutesParams {
    pub fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        push_opt(&mut parts, "group", self.group.as_deref());
        push_opt(&mut parts, "owner_id", self.owner_id.as_deref());
        push_opt(&mut parts, "method", self.method.as_deref());
        push_opt(&mut parts, "path_pattern", self.path_pattern.as_deref());
        push_opt(&mut parts, "since", self.since.as_deref());
        push_opt(&mut parts, "until", self.until.as_deref());
        push_opt(&mut parts, "q", self.q.as_deref());
        push_opt(&mut parts, "sort", self.sort.as_deref());
        push_opt(&mut parts, "dir", self.dir.as_deref());
        push_opt_num(&mut parts, "offset", self.offset);
        push_opt_num(&mut parts, "limit", self.limit);
        parts.join("&")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListJournalParams {
    pub before: Option<u32>,
    pub limit: Option<usize>,
    pub route: Option<String>,
    pub method: Option<String>,
    pub path_pattern: Option<String>,
    pub status: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

impl ListJournalParams {
    pub fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        push_opt_num(&mut parts, "before", self.before.map(|n| n as u64));
        push_opt_num(&mut parts, "limit", self.limit.map(|n| n as u64));
        push_opt(&mut parts, "route", self.route.as_deref());
        push_opt(&mut parts, "method", self.method.as_deref());
        push_opt(&mut parts, "path_pattern", self.path_pattern.as_deref());
        push_opt(&mut parts, "status", self.status.as_deref());
        push_opt(&mut parts, "since", self.since.as_deref());
        push_opt(&mut parts, "until", self.until.as_deref());
        parts.join("&")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListUnmatchedParams {
    pub before: Option<u64>,
    pub limit: Option<usize>,
    pub method: Option<String>,
    pub path_pattern: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

impl ListUnmatchedParams {
    pub fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        push_opt_num(&mut parts, "before", self.before);
        push_opt_num(&mut parts, "limit", self.limit.map(|n| n as u64));
        push_opt(&mut parts, "method", self.method.as_deref());
        push_opt(&mut parts, "path_pattern", self.path_pattern.as_deref());
        push_opt(&mut parts, "since", self.since.as_deref());
        push_opt(&mut parts, "until", self.until.as_deref());
        parts.join("&")
    }
}

fn push_opt(parts: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        parts.push(format!("{key}={}", urlencode_param(v)));
    }
}

fn push_opt_num<N: std::fmt::Display>(parts: &mut Vec<String>, key: &str, value: Option<N>) {
    if let Some(v) = value {
        parts.push(format!("{key}={v}"));
    }
}

/// Minimal percent-encoding for query-string values — covers the
/// characters that would otherwise break parsing (`&`, `=`, ` `, `+`,
/// `?`, `#`) plus non-ASCII bytes. We deliberately don't reach for
/// `urlencoding` or `url` to keep wm-core's dependency list lean —
/// the alphabet of filter values is well-controlled.
fn urlencode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b'*' => {
                out.push(*b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
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
    /// Scopes granted to this token. v1 always returns `["*"]` (full
    /// access of the owner); the field is reserved per ADR-0012 so
    /// v0.2 can wire up enforcement without a data-shape change.
    pub scopes: Vec<String>,
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

// -- Users -------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserRecord {
    pub id: String,
    pub name: String,
    pub is_admin: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListUsersResponse {
    pub users: Vec<UserRecord>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateUserBody {
    pub name: String,
    #[serde(default)]
    pub is_admin: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PatchUserBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_admin: Option<bool>,
}

// -- Match probe -------------------------------------------------------------

/// Response from `GET /__api/match`. Either a hit (with the matched
/// route) or a miss (with the list of near-misses).
///
/// Both variants are boxed so the enum doesn't lopsidedly pay for
/// the larger `MatchHit` (which carries a full `RouteRecord`) on
/// every miss.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MatchResponse {
    Hit(Box<MatchHit>),
    Miss(Box<MatchMiss>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MatchHit {
    /// Always `true`; carried as a flag so the wire shape is
    /// self-describing without relying on the enum tag.
    pub matched: bool,
    pub route: RouteRecord,
    pub path_params: Vec<(String, String)>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MatchMiss {
    /// Always `false`; carried for the same reason as on `MatchHit`.
    pub matched: bool,
    pub near_misses: Vec<NearMiss>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NearMiss {
    /// `{group}/{n}` slug.
    pub route: String,
    pub route_id: String,
    pub route_path: String,
    pub reason: NearMissReason,
    /// Free-form details. Shape depends on `reason`; for
    /// `method_mismatch` it's `{expected_methods, got}`, for
    /// `prefix_match` it's `{segment_index, expected, got}`.
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NearMissReason {
    MethodMismatch,
    PrefixMatch,
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

// -- Capabilities ------------------------------------------------------------

/// Response from `GET /__api/capabilities[/{topic}]`. Markdown
/// documentation for the WireMirage handler API; same content the
/// MCP `get_capabilities` tool returns.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CapabilityResponse {
    /// Resolved topic name. Echoes the request when known, "overview"
    /// when the requested topic was empty or unknown.
    pub topic: String,
    /// The markdown body for this topic.
    pub content: String,
    /// All topic names the host knows about.
    pub available_topics: Vec<String>,
}
