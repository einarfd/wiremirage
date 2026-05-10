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
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WhoAmIUser {
    pub id: String,
    pub name: String,
    pub is_admin: bool,
}

#[tool_router(router = identity_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "who_am_i",
        description = "Show the user identity associated with the current MCP session's bearer token: id, name, and admin status. Use this to verify which user the MCP token authenticates as, especially in shared CI environments."
    )]
    pub async fn who_am_i(
        &self,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<Json<WhoAmIResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        Ok(Json(WhoAmIResult {
            user: WhoAmIUser {
                id: auth.user_id,
                name: auth.user_name,
                is_admin: auth.is_admin,
            },
        }))
    }
}
