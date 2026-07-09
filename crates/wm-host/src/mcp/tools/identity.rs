//! Identity tools — `who_am_i`.

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::context::auth_from;
use crate::mcp::server::WmMcpServer;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WhoAmIResult {
    pub user: WhoAmIUser,
    /// The public base URL this host is reached at (e.g.
    /// `https://wm.example.com`) — derived from the request, honoring
    /// `X-Forwarded-*` when behind a trusted proxy. Mock
    /// routes are served directly under it: a route at `/v1/charges`
    /// answers at `{base_url}/v1/charges`. The `/api/mcp` endpoint
    /// you're talking to right now is `{base_url}/api/mcp`.
    pub base_url: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WhoAmIUser {
    pub id: String,
    /// The account identifier: a verified email, also the
    /// cross-provider identity-linking key (user-model.md).
    pub email: String,
    pub is_admin: bool,
}

#[tool_router(router = identity_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "who_am_i",
        description = "Show the user identity associated with the current MCP session's bearer token: id, email, and admin status. Use this to verify which user the MCP token authenticates as, especially in shared CI environments."
    )]
    pub async fn who_am_i(
        &self,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<Json<WhoAmIResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let base_url =
            crate::auth_api::public_base_url(&parts.headers, self.state.trust_forwarded_headers());
        Ok(Json(WhoAmIResult {
            user: WhoAmIUser {
                id: auth.user_id,
                email: auth.user_email,
                is_admin: auth.is_admin,
            },
            base_url,
        }))
    }
}
