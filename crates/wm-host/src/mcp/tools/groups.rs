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

use crate::api::{
    group_matches_since_until, parse_group_sort, parse_pagination, slice_for_page, sort_groups,
};
use crate::api_filters::{FilterParseError, SortDir, parse_since};
use crate::mcp::context::{auth_from, ensure_group_owner_or_admin};
use crate::mcp::error::{map_filter_error, map_registry_error, validation};
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

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct ListGroupsArgs {
    /// Restrict to groups owned by this user (admin-only). Non-admin
    /// callers always see only their own groups.
    pub owner_id: Option<String>,
    /// Match groups whose name starts with this prefix.
    pub name_prefix: Option<String>,
    /// Free-text needle (case-insensitive substring over name).
    pub q: Option<String>,
    /// Lower bound on `last_activity_at`. Duration suffix or RFC 3339.
    pub since: Option<String>,
    pub until: Option<String>,
    /// Show only implicit (`true`) or only explicit (`false`) groups.
    pub implicit: Option<bool>,
    /// Sort column: `created_at` (default), `name`, `last_activity_at`.
    pub sort: Option<String>,
    /// Sort direction: `asc` or `desc`. Default `desc`.
    pub dir: Option<String>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListGroupsResult {
    pub groups: Vec<GroupRecord>,
    pub total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowGroupArgs {
    /// Group name or ULID.
    pub group: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CreateGroupArgs {
    /// Canonical group name; doubles as the group's subdomain, so it must
    /// be a valid DNS label (lowercase a-z, 0-9, hyphen; no leading/trailing
    /// hyphen). Optional — omit to be assigned a friendly name (ADR-0030).
    /// Must be unique. Used in route slugs.
    pub name: Option<String>,
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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateGroupArgs {
    /// Group name or ULID.
    pub group: String,
    /// New TTL in seconds. Capped at the host's `MAX_GROUP_TTL_SECONDS`
    /// (30d). Omit to leave the TTL alone. Setting this re-arms the
    /// Valkey TTL on the group record.
    pub ttl_seconds: Option<u64>,
    /// Flip the sliding-TTL flag. `true` = every dispatched request
    /// bumps the group's expiry; `false` = fixed-window expiry from
    /// the last refresh. Omit to leave the flag alone.
    pub sliding_ttl: Option<bool>,
}

#[tool_router(router = groups_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "list_groups",
        description = "List groups with optional filters / sort / pagination. Filter by admin-only `owner_id`, `name_prefix`, free-text `q`, `since`/`until` against `last_activity_at`, or `implicit`. Sort by `created_at` (default), `name`, or `last_activity_at`. Paginate with `offset` + `limit` (default 50, max 200). Response carries `total` + `next_offset`. Non-admin always sees only their own groups."
    )]
    pub async fn list_groups(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListGroupsArgs>,
    ) -> Result<Json<ListGroupsResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (offset, limit) =
            parse_pagination(args.offset, args.limit).map_err(map_filter_error)?;
        let dir = SortDir::parse(args.dir.as_deref(), SortDir::Desc).map_err(map_filter_error)?;
        let sort_key = parse_group_sort(args.sort.as_deref()).map_err(map_filter_error)?;
        let now = chrono::Utc::now();
        let since = args
            .since
            .as_deref()
            .map(|s| parse_since(s, now))
            .transpose()
            .map_err(map_filter_error)?;
        let until = args
            .until
            .as_deref()
            .map(|s| parse_since(s, now))
            .transpose()
            .map_err(map_filter_error)?;

        let owner_filter: Option<String> = if auth.is_admin {
            args.owner_id.clone()
        } else {
            if args.owner_id.is_some() {
                return Err(map_filter_error(FilterParseError::OwnerNonAdmin));
            }
            Some(auth.user_id.clone())
        };

        let groups = match owner_filter.as_deref() {
            Some(owner) => self
                .state
                .routes()
                .registry()
                .list_groups_by_owner(owner)
                .map_err(map_registry_error)?,
            None => self
                .state
                .routes()
                .registry()
                .list_groups()
                .map_err(map_registry_error)?,
        };

        let mut filtered: Vec<Group> = groups
            .into_iter()
            .filter(|g| match args.name_prefix.as_deref() {
                Some(p) => g.name.starts_with(p),
                None => true,
            })
            .filter(|g| match args.q.as_deref() {
                Some(needle) => g
                    .name
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase()),
                None => true,
            })
            .filter(|g| group_matches_since_until(g, since, until))
            .filter(|g| match args.implicit {
                Some(want) => g.implicit == want,
                None => true,
            })
            .collect();

        sort_groups(&mut filtered, sort_key, dir);

        let (page, total, next_offset) = slice_for_page(&filtered, offset, limit);
        Ok(Json(ListGroupsResult {
            groups: page.iter().map(GroupRecord::from).collect(),
            total,
            next_offset,
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
                // Empty/omitted name → registry auto-assigns a friendly one.
                name: args.name.unwrap_or_default(),
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

    #[tool(
        name = "update_group",
        description = "Update a group's mutable fields by name or ULID: `ttl_seconds` (re-arms the Valkey TTL) and/or `sliding_ttl` (toggle the sliding-expiry flag). Owner-or-admin only. At least one of the two fields must be set. Rename and owner-transfer aren't supported through this tool."
    )]
    pub async fn update_group(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateGroupArgs>,
    ) -> Result<Json<GroupRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let group = ensure_group_owner_or_admin(&self.state, &auth, &args.group)?;
        if args.ttl_seconds.is_none() && args.sliding_ttl.is_none() {
            return Err(validation(
                "update_group needs at least one of `ttl_seconds` or `sliding_ttl`",
            ));
        }
        let updated = self
            .state
            .routes()
            .registry()
            .patch_group(&group.id, args.ttl_seconds, args.sliding_ttl)
            .map_err(map_registry_error)?;
        Ok(Json(GroupRecord::from(&updated)))
    }
}
