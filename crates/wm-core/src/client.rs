//! Typed REST client for the wm-host API.
//!
//! Built on `reqwest`. Always async — sync call sites bring up a small
//! tokio runtime in their entry point. Errors are translated from HTTP
//! status to typed variants so downstream consumers (CLI, MCP) can map
//! them to user-facing exit codes / structured tool responses without
//! re-parsing JSON in each caller.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::models::{
    ApiErrorBody, CreateGroupBody, CreateRouteBody, CreateTokenBody, CreateTokenResponse,
    DryRunBody, DryRunResult, GroupRecord, HealthResponse, JournalRecord, ListGroupsParams,
    ListGroupsResponse, ListJournalParams, ListJournalResponse, ListRouteStateResponse,
    ListRoutesParams, ListRoutesResponse, ListTokensResponse, ListUnmatchedParams,
    ListUnmatchedResponse, PatchGroupBody, PatchRouteBody, ReadyResponse, RouteRecord,
    RouteSourceResponse, UnmatchedRecord,
};

const DEFAULT_USER_AGENT: &str = concat!("wm-cli/", env!("CARGO_PKG_VERSION"));
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("network error: {0}")]
    Network(String),
    #[error("invalid host URL: {0}")]
    InvalidHost(String),
    #[error("server returned non-JSON or unrecognised response: {0}")]
    BadResponse(String),
    /// 401 — token missing / invalid / expired. Maps to CLI exit code 4.
    #[error("authentication failed: {0}")]
    Unauthorized(String),
    /// 403 — token authenticated but not allowed. Maps to CLI exit code 4.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// 404 — resource missing. Maps to CLI exit code 5.
    #[error("not found: {0}")]
    NotFound(String),
    /// 409 — name in use, route conflict, etc. Maps to CLI exit code 6.
    #[error("conflict: {0}")]
    Conflict(String),
    /// 400 — request rejected as malformed/invalid. Maps to CLI exit code 1.
    #[error("validation failed: {0}")]
    Validation(String),
    /// Anything else from the host (5xx, unexpected 4xx). Maps to CLI exit code 1.
    #[error("server error ({status}): {message}")]
    ServerError { status: u16, message: String },
}

/// Builder for `Client`. Use `Client::builder(host)` and chain
/// `with_token(...)` / `with_user_agent(...)` etc.
pub struct ClientBuilder {
    host: String,
    token: Option<String>,
    user_agent: String,
    request_timeout: Duration,
}

impl ClientBuilder {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            token: None,
            user_agent: DEFAULT_USER_AGENT.into(),
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Override the default `wm-cli/{version}` User-Agent. Used by
    /// non-CLI tooling (`wm-mcp`, future scripts) so traffic is
    /// labelled by client in the host's logs and OTel spans.
    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn build(self) -> Result<Client, ClientError> {
        let mut headers = HeaderMap::new();
        let ua = HeaderValue::from_str(&self.user_agent)
            .map_err(|e| ClientError::InvalidHost(format!("user agent: {e}")))?;
        headers.insert(reqwest::header::USER_AGENT, ua);
        if let Some(token) = &self.token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| ClientError::InvalidHost(format!("token: {e}")))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(self.request_timeout)
            .build()
            .map_err(|e| ClientError::InvalidHost(format!("http client: {e}")))?;
        let host = self.host.trim_end_matches('/').to_string();
        Ok(Client { http, host })
    }
}

/// Typed REST client. Cheap to clone (the inner reqwest client is
/// internally Arc-backed).
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    host: String,
}

impl Client {
    pub fn builder(host: impl Into<String>) -> ClientBuilder {
        ClientBuilder::new(host)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.host, path)
    }

    // -- Health ---------------------------------------------------------

    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        self.send(Method::GET, "/__health", None::<&()>).await
    }

    pub async fn ready(&self) -> Result<ReadyResponse, ClientError> {
        self.send(Method::GET, "/__ready", None::<&()>).await
    }

    // -- Groups ---------------------------------------------------------

    pub async fn list_groups(&self) -> Result<ListGroupsResponse, ClientError> {
        self.list_groups_with(&ListGroupsParams::default()).await
    }

    /// List groups with explicit filter / sort / pagination params.
    /// All fields on `params` are optional; an all-`None` params behaves
    /// identically to `list_groups()`.
    pub async fn list_groups_with(
        &self,
        params: &ListGroupsParams,
    ) -> Result<ListGroupsResponse, ClientError> {
        let qs = params.to_query_string();
        let path = if qs.is_empty() {
            "/__api/groups".to_string()
        } else {
            format!("/__api/groups?{qs}")
        };
        self.send(Method::GET, &path, None::<&()>).await
    }

    pub async fn create_group(&self, body: &CreateGroupBody) -> Result<GroupRecord, ClientError> {
        self.send(Method::POST, "/__api/groups", Some(body)).await
    }

    pub async fn get_group(&self, group: &str) -> Result<GroupRecord, ClientError> {
        self.send(
            Method::GET,
            &format!("/__api/groups/{}", urlencode(group)),
            None::<&()>,
        )
        .await
    }

    pub async fn patch_group(
        &self,
        group: &str,
        body: &PatchGroupBody,
    ) -> Result<GroupRecord, ClientError> {
        self.send(
            Method::PATCH,
            &format!("/__api/groups/{}", urlencode(group)),
            Some(body),
        )
        .await
    }

    pub async fn delete_group(&self, group: &str) -> Result<(), ClientError> {
        self.send_no_body(
            Method::DELETE,
            &format!("/__api/groups/{}", urlencode(group)),
        )
        .await
    }

    pub async fn refresh_group(&self, group: &str) -> Result<GroupRecord, ClientError> {
        self.send(
            Method::POST,
            &format!("/__api/groups/{}/refresh", urlencode(group)),
            None::<&()>,
        )
        .await
    }

    pub async fn clear_group_state(&self, group: &str) -> Result<(), ClientError> {
        self.send_no_body(
            Method::DELETE,
            &format!("/__api/groups/{}/state", urlencode(group)),
        )
        .await
    }

    pub async fn clear_group_journal(&self, group: &str) -> Result<(), ClientError> {
        self.send_no_body(
            Method::DELETE,
            &format!("/__api/groups/{}/journal", urlencode(group)),
        )
        .await
    }

    // -- Routes ---------------------------------------------------------

    pub async fn list_routes(&self) -> Result<ListRoutesResponse, ClientError> {
        self.list_routes_with(&ListRoutesParams::default()).await
    }

    pub async fn list_routes_with(
        &self,
        params: &ListRoutesParams,
    ) -> Result<ListRoutesResponse, ClientError> {
        let qs = params.to_query_string();
        let path = if qs.is_empty() {
            "/__api/routes".to_string()
        } else {
            format!("/__api/routes?{qs}")
        };
        self.send(Method::GET, &path, None::<&()>).await
    }

    pub async fn create_route(&self, body: &CreateRouteBody) -> Result<RouteRecord, ClientError> {
        self.send(Method::POST, "/__api/routes", Some(body)).await
    }

    pub async fn get_route(&self, slug: &str) -> Result<RouteRecord, ClientError> {
        // slug is `{group}/{n}`; split-and-encode so a group name
        // containing reserved URL chars (none today, but be defensive)
        // doesn't break the URL.
        let (group, number) = split_route_slug(slug)?;
        self.send(
            Method::GET,
            &format!("/__api/routes/{}/{number}", urlencode(group)),
            None::<&()>,
        )
        .await
    }

    pub async fn patch_route(
        &self,
        slug: &str,
        body: &PatchRouteBody,
    ) -> Result<RouteRecord, ClientError> {
        let (group, number) = split_route_slug(slug)?;
        self.send(
            Method::PATCH,
            &format!("/__api/routes/{}/{number}", urlencode(group)),
            Some(body),
        )
        .await
    }

    pub async fn delete_route(&self, slug: &str) -> Result<(), ClientError> {
        let (group, number) = split_route_slug(slug)?;
        self.send_no_body(
            Method::DELETE,
            &format!("/__api/routes/{}/{number}", urlencode(group)),
        )
        .await
    }

    pub async fn list_route_state(
        &self,
        slug: &str,
    ) -> Result<ListRouteStateResponse, ClientError> {
        let (group, number) = split_route_slug(slug)?;
        self.send(
            Method::GET,
            &format!("/__api/routes/{}/{number}/state", urlencode(group)),
            None::<&()>,
        )
        .await
    }

    pub async fn clear_route_state(&self, slug: &str) -> Result<(), ClientError> {
        let (group, number) = split_route_slug(slug)?;
        self.send_no_body(
            Method::DELETE,
            &format!("/__api/routes/{}/{number}/state", urlencode(group)),
        )
        .await
    }

    pub async fn get_route_source(&self, slug: &str) -> Result<RouteSourceResponse, ClientError> {
        let (group, number) = split_route_slug(slug)?;
        self.send(
            Method::GET,
            &format!("/__api/routes/{}/{number}/source", urlencode(group)),
            None::<&()>,
        )
        .await
    }

    pub async fn dry_run_route(
        &self,
        slug: &str,
        body: &DryRunBody,
    ) -> Result<DryRunResult, ClientError> {
        let (group, number) = split_route_slug(slug)?;
        self.send(
            Method::POST,
            &format!("/__api/routes/{}/{number}/dry-run", urlencode(group)),
            Some(body),
        )
        .await
    }

    // -- Journal --------------------------------------------------------

    pub async fn list_journal(
        &self,
        group: &str,
        before: Option<u32>,
        limit: Option<usize>,
    ) -> Result<ListJournalResponse, ClientError> {
        self.list_journal_with(
            group,
            &ListJournalParams {
                before,
                limit,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn list_journal_with(
        &self,
        group: &str,
        params: &ListJournalParams,
    ) -> Result<ListJournalResponse, ClientError> {
        let qs = params.to_query_string();
        let path = if qs.is_empty() {
            format!("/__api/journal/{}", urlencode(group))
        } else {
            format!("/__api/journal/{}?{qs}", urlencode(group))
        };
        self.send(Method::GET, &path, None::<&()>).await
    }

    /// List unmatched-request entries. Admin-only on the host side.
    pub async fn list_unmatched(
        &self,
        params: &ListUnmatchedParams,
    ) -> Result<ListUnmatchedResponse, ClientError> {
        let qs = params.to_query_string();
        let path = if qs.is_empty() {
            "/__api/unmatched".to_string()
        } else {
            format!("/__api/unmatched?{qs}")
        };
        self.send(Method::GET, &path, None::<&()>).await
    }

    /// Show one unmatched entry by its host-wide number. Admin-only.
    pub async fn get_unmatched_entry(&self, number: u64) -> Result<UnmatchedRecord, ClientError> {
        self.send(
            Method::GET,
            &format!("/__api/unmatched/{number}"),
            None::<&()>,
        )
        .await
    }

    pub async fn get_journal_entry(
        &self,
        group: &str,
        number: u32,
    ) -> Result<JournalRecord, ClientError> {
        self.send(
            Method::GET,
            &format!("/__api/journal/{}/{number}", urlencode(group)),
            None::<&()>,
        )
        .await
    }

    // -- Tokens ---------------------------------------------------------

    pub async fn list_tokens(&self) -> Result<ListTokensResponse, ClientError> {
        self.send(Method::GET, "/__api/tokens", None::<&()>).await
    }

    pub async fn create_token(
        &self,
        body: &CreateTokenBody,
    ) -> Result<CreateTokenResponse, ClientError> {
        self.send(Method::POST, "/__api/tokens", Some(body)).await
    }

    pub async fn delete_token(&self, name: &str) -> Result<(), ClientError> {
        self.send_no_body(
            Method::DELETE,
            &format!("/__api/tokens/{}", urlencode(name)),
        )
        .await
    }

    // -- Users ----------------------------------------------------------

    /// List all users. Admin-only on the host side; non-admin callers
    /// get a `Forbidden` error.
    pub async fn list_users(&self) -> Result<crate::models::ListUsersResponse, ClientError> {
        self.send(Method::GET, "/__api/users", None::<&()>).await
    }

    /// Show one user by name. Admin can see any user; non-admin can
    /// see their own record only (the host enforces).
    pub async fn get_user(&self, name: &str) -> Result<crate::models::UserRecord, ClientError> {
        self.send(
            Method::GET,
            &format!("/__api/users/{}", urlencode(name)),
            None::<&()>,
        )
        .await
    }

    /// Show the authenticated user's own record. Always available
    /// regardless of admin status.
    pub async fn get_me(&self) -> Result<crate::models::UserRecord, ClientError> {
        self.send(Method::GET, "/__api/users/me", None::<&()>).await
    }

    /// Create a new user. Admin-only.
    pub async fn create_user(
        &self,
        body: &crate::models::CreateUserBody,
    ) -> Result<crate::models::UserRecord, ClientError> {
        self.send(Method::POST, "/__api/users", Some(body)).await
    }

    /// Update a user's mutable fields (currently only `is_admin`).
    /// Admin-only for cross-user updates.
    pub async fn patch_user(
        &self,
        name: &str,
        body: &crate::models::PatchUserBody,
    ) -> Result<crate::models::UserRecord, ClientError> {
        self.send(
            Method::PATCH,
            &format!("/__api/users/{}", urlencode(name)),
            Some(body),
        )
        .await
    }

    /// Delete a user. Admin-only. The host refuses to delete the
    /// last admin or a user that owns routes.
    pub async fn delete_user(&self, name: &str) -> Result<(), ClientError> {
        self.send_no_body(Method::DELETE, &format!("/__api/users/{}", urlencode(name)))
            .await
    }

    // -- Capabilities ---------------------------------------------------

    /// Fetch the handler-API documentation. `topic = None` returns
    /// the overview + topic list. Unknown topics fall back to the
    /// overview server-side, matching the MCP tool's behaviour.
    pub async fn capabilities(
        &self,
        topic: Option<&str>,
    ) -> Result<crate::models::CapabilityResponse, ClientError> {
        let path = match topic {
            Some(t) if !t.is_empty() => format!("/__api/capabilities/{}", urlencode(t)),
            _ => "/__api/capabilities".to_string(),
        };
        self.send(Method::GET, &path, None::<&()>).await
    }

    // -- Match probe ----------------------------------------------------

    /// Probe what would match a hypothetical request. Returns either
    /// the route that would handle it (`MatchResponse::Hit`) or a
    /// list of near-misses (`MatchResponse::Miss`).
    pub async fn match_route(
        &self,
        method: &str,
        path: &str,
    ) -> Result<crate::models::MatchResponse, ClientError> {
        let qs = format!(
            "/__api/match?method={}&path={}",
            urlencode(method),
            urlencode(path),
        );
        self.send(Method::GET, &qs, None::<&()>).await
    }

    // -- Generic plumbing -----------------------------------------------

    async fn send<B: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<R, ClientError> {
        let url = self.url(path);
        let mut builder = self.http.request(method, &url);
        if let Some(b) = body {
            builder = builder.json(b);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ClientError::Network(e.to_string()))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ClientError::Network(format!("read body: {e}")))?;
        if status.is_success() {
            return serde_json::from_slice(&bytes)
                .map_err(|e| ClientError::BadResponse(format!("decode body: {e}")));
        }
        Err(translate_error(status, &bytes))
    }

    async fn send_no_body(&self, method: Method, path: &str) -> Result<(), ClientError> {
        let url = self.url(path);
        let response = self
            .http
            .request(method, &url)
            .send()
            .await
            .map_err(|e| ClientError::Network(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ClientError::Network(format!("read body: {e}")))?;
        Err(translate_error(status, &bytes))
    }
}

fn translate_error(status: StatusCode, bytes: &[u8]) -> ClientError {
    let parsed: Option<ApiErrorBody> = serde_json::from_slice(bytes).ok();
    let message = parsed
        .as_ref()
        .map(|b| b.error.message.clone())
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
    match status {
        StatusCode::UNAUTHORIZED => ClientError::Unauthorized(message),
        StatusCode::FORBIDDEN => ClientError::Forbidden(message),
        StatusCode::NOT_FOUND => ClientError::NotFound(message),
        StatusCode::CONFLICT => ClientError::Conflict(message),
        StatusCode::BAD_REQUEST => ClientError::Validation(message),
        other => ClientError::ServerError {
            status: other.as_u16(),
            message,
        },
    }
}

fn split_route_slug(slug: &str) -> Result<(&str, u32), ClientError> {
    let (group, n) = slug.split_once('/').ok_or_else(|| {
        ClientError::Validation(format!(
            "route slug must be in the form 'group/N', got {slug:?}"
        ))
    })?;
    let n = n.parse::<u32>().map_err(|e| {
        ClientError::Validation(format!("route slug 'group/N': N must be u32 ({e})"))
    })?;
    Ok((group, n))
}

/// Minimal URL-encoder for path segments. Group/token names are
/// constrained by the host (`storage-model.md` allows letters, digits,
/// hyphens, underscores; user-supplied names like token names can
/// contain spaces in principle), so we percent-encode anything that
/// isn't unreserved-by-RFC-3986 to be safe.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_slug_happy_path() {
        let (g, n) = split_route_slug("stripe-mock/7").unwrap();
        assert_eq!(g, "stripe-mock");
        assert_eq!(n, 7);
    }

    #[test]
    fn split_slug_rejects_missing_slash() {
        let err = split_route_slug("just-a-name").unwrap_err();
        assert!(matches!(err, ClientError::Validation(_)));
    }

    #[test]
    fn split_slug_rejects_non_integer() {
        let err = split_route_slug("group/abc").unwrap_err();
        assert!(matches!(err, ClientError::Validation(_)));
    }

    #[test]
    fn urlencode_passes_through_unreserved_chars() {
        assert_eq!(urlencode("stripe-mock_v2"), "stripe-mock_v2");
    }

    #[test]
    fn urlencode_escapes_spaces_and_specials() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }
}
