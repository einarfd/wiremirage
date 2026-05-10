//! State tools — `clear_group_state`. Per-route variant
//! (`clear_route_state`) is deferred until the host adds a per-route
//! state endpoint; the cli-design and mcp-surface docs both flag it.

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::context::{auth_from, ensure_group_owner_or_admin};
use crate::mcp::error::map_registry_error;
use crate::mcp::server::WmMcpServer;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearGroupStateArgs {
    /// Group name or ULID.
    pub group: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearGroupStateResult {
    pub cleared: bool,
}

#[tool_router(router = state_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "clear_group_state",
        description = "Clear all per-route stores in a group plus the group's shared store. Routes themselves stay alive. Use between test phases when the whole group's state should reset."
    )]
    pub async fn clear_group_state(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ClearGroupStateArgs>,
    ) -> Result<Json<ClearGroupStateResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        self.state
            .routes()
            .registry()
            .clear_group_state(&group.id)
            .map_err(map_registry_error)?;
        Ok(Json(ClearGroupStateResult { cleared: true }))
    }
}
