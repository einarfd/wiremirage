//! Capabilities tool — `get_capabilities`. Returns markdown
//! documentation for the WireMirage handler API. Content lives in the
//! crate-level `crate::capabilities` module so the same strings back
//! the REST endpoint and the `wm capabilities` CLI command.
//!
//! We expose this as an MCP *tool* rather than a *resource* because
//! resource support across MCP clients is uneven (Claude Desktop in
//! particular). arkiv made the same call.

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::capabilities;
use crate::mcp::server::WmMcpServer;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetCapabilitiesArgs {
    /// Topic name. Omit to get the overview + list of topics. Known
    /// topics: `overview`, `request`, `response`, `store`, `log`,
    /// `clock`, `gotchas`.
    pub topic: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CapabilitiesResult {
    /// The topic name that was returned. Echoes the request when
    /// known, "overview" when the request was empty or unknown.
    pub topic: String,
    /// The markdown body for the topic.
    pub content: String,
    /// All topic names this tool knows about. Includes the one in
    /// `topic` field.
    pub available_topics: Vec<String>,
}

#[tool_router(router = capabilities_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "get_capabilities",
        description = "Return markdown documentation for the WireMirage handler API: how to author the `source` field of `create_route`, the request/response shape, the per-route and per-group stores, log, clock primitives, and common gotchas. Call without arguments for an overview + list of available topics; call with `topic` to get a specific section. Call this BEFORE writing your first handler — the rest of the MCP tool descriptions tell you how to manage routes, not what the handler code itself should look like."
    )]
    pub async fn get_capabilities(
        &self,
        Parameters(args): Parameters<GetCapabilitiesArgs>,
    ) -> Result<Json<CapabilitiesResult>, ErrorData> {
        let (topic, content) = capabilities::lookup(args.topic.as_deref());
        Ok(Json(CapabilitiesResult {
            topic: topic.to_string(),
            content: content.to_string(),
            available_topics: capabilities::topic_names()
                .into_iter()
                .map(String::from)
                .collect(),
        }))
    }
}
