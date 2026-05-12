//! Group tools — `list_groups`, `show_group`, `create_group`,
//! `delete_group`, `refresh_group_ttl`.

use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::context::{auth_from, ensure_group_owner_or_admin};
use crate::mcp::error::{map_registry_error, validation};
use crate::mcp::server::WmMcpServer;
use crate::registry::{Group, NewGroup};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GroupRecord {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub ttl_seconds: u64,
    pub sliding_ttl: bool,
    pub implicit: bool,
    pub created_at: String,
    /// Most recent matched dispatch against any route in the group.
    /// `None` for groups that have never seen traffic.
    pub last_activity_at: Option<String>,
}

impl From<&Group> for GroupRecord {
    fn from(g: &Group) -> Self {
        Self {
            id: g.id.clone(),
            name: g.name.clone(),
            owner_id: g.owner_id.clone(),
            ttl_seconds: g.ttl_seconds,
            sliding_ttl: g.sliding_ttl,
            implicit: g.implicit,
            created_at: g.created_at.to_rfc3339(),
            last_activity_at: g.last_activity_at.map(|ts| ts.to_rfc3339()),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListGroupsResult {
    pub groups: Vec<GroupRecord>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowGroupArgs {
    /// Group name or ULID.
    pub group: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CreateGroupArgs {
    /// Canonical group name. Must be unique. Used in route slugs.
    pub name: String,
    /// TTL in seconds. Defaults to 24h. Capped at 30d.
    pub ttl_seconds: Option<u64>,
    /// When `true`, every successful route hit bumps the group's
    /// expiry. Defaults to `true`.
    pub sliding_ttl: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DeleteGroupArgs {
    /// Group name or ULID.
    pub group: String,
    /// Required guard against accidental deletion. Must be `true`.
    pub confirm: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DeleteGroupResult {
    pub deleted: bool,
    pub routes_deleted: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RefreshGroupArgs {
    /// Group name or ULID.
    pub group: String,
}

#[tool_router(router = groups_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "list_groups",
        description = "List groups visible to the authenticated user. Non-admin sees only groups they own; admin sees all."
    )]
    pub async fn list_groups(
        &self,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<Json<ListGroupsResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let groups = if auth.is_admin {
            self.state
                .routes()
                .registry()
                .list_groups()
                .map_err(map_registry_error)?
        } else {
            self.state
                .routes()
                .registry()
                .list_groups_by_owner(&auth.user_id)
                .map_err(map_registry_error)?
        };
        Ok(Json(ListGroupsResult {
            groups: groups.iter().map(GroupRecord::from).collect(),
        }))
    }

    #[tool(
        name = "show_group",
        description = "Show full details of a group: name, owner, TTL, sliding flag, creation time. Use to understand what a group is before modifying it."
    )]
    pub async fn show_group(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ShowGroupArgs>,
    ) -> Result<Json<GroupRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        Ok(Json(GroupRecord::from(&group)))
    }

    #[tool(
        name = "create_group",
        description = "Create a new group. Groups are lifecycle units: when the group expires (TTL) or is deleted, all routes inside it are wiped along with kv/gkv state and journal entries."
    )]
    pub async fn create_group(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CreateGroupArgs>,
    ) -> Result<Json<GroupRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = self
            .state
            .routes()
            .registry()
            .create_group(NewGroup {
                name: args.name,
                owner_id: auth.user_id,
                ttl_seconds: args.ttl_seconds,
                sliding_ttl: args.sliding_ttl,
            })
            .map_err(map_registry_error)?;
        Ok(Json(GroupRecord::from(&group)))
    }

    #[tool(
        name = "delete_group",
        description = "Delete a group, cascading routes, kv/gkv state, and journal entries. The `confirm` parameter must be `true` — guards against accidental deletion in agent workflows."
    )]
    pub async fn delete_group(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<DeleteGroupArgs>,
    ) -> Result<Json<DeleteGroupResult>, ErrorData> {
        if !args.confirm {
            return Err(validation(
                "delete_group requires `confirm: true` — set it explicitly to proceed",
            ));
        }
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        let routes_deleted = self
            .state
            .routes()
            .registry()
            .cascade_delete_group(&group.id)
            .map_err(map_registry_error)?;
        self.state.routes().refresh_after_group_cascade(&group.id);
        Ok(Json(DeleteGroupResult {
            deleted: true,
            routes_deleted,
        }))
    }

    #[tool(
        name = "refresh_group_ttl",
        description = "Bump a group's expiry forward by its configured TTL. Use when a non-sliding group is about to expire and you want to extend it without changing the TTL value."
    )]
    pub async fn refresh_group_ttl(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RefreshGroupArgs>,
    ) -> Result<Json<GroupRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        let refreshed = self
            .state
            .routes()
            .registry()
            .refresh_group(&group.id)
            .map_err(map_registry_error)?;
        Ok(Json(GroupRecord::from(&refreshed)))
    }
}
