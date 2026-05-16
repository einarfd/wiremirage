//! Route tools — `list_routes`, `show_route`, `create_route`,
//! `delete_route`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rmcp::ErrorData;
use rmcp::Json;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SUPPORTED_BINDINGS_VERSION;
use crate::api::{
    parse_pagination, parse_route_sort, route_matches_q, route_matches_since_until, slice_for_page,
    sort_routes,
};
use crate::api_filters::{FilterParseError, SortDir, glob_match, parse_since, validate_method};
use crate::mcp::context::{auth_from, ensure_route_owner_or_admin};
use crate::mcp::error::{forbidden, map_registry_error, validation};
use crate::mcp::server::WmMcpServer;
use crate::registry::{NewRoute, PatchRoute, Route, render_slug};

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RouteRecord {
    pub id: String,
    pub number: u32,
    pub group: GroupRef,
    pub methods: Vec<String>,
    pub path: String,
    pub language: String,
    pub bindings_version: String,
    pub owner_id: String,
    pub created_at: String,
    /// Cumulative count of matched dispatches against this route.
    pub hits_total: u64,
    /// Most recent matched dispatch against this route. `None` for
    /// never-hit routes.
    pub last_hit_at: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GroupRef {
    pub id: String,
    pub name: String,
}

impl From<&Route> for RouteRecord {
    fn from(r: &Route) -> Self {
        Self {
            id: r.id.clone(),
            number: r.number,
            group: GroupRef {
                id: r.group_id.clone(),
                name: r.group_name.clone(),
            },
            methods: r.methods.clone(),
            path: r.path.clone(),
            language: r.language.clone(),
            bindings_version: r.bindings_version.clone(),
            owner_id: r.owner_id.clone(),
            created_at: r.created_at.to_rfc3339(),
            hits_total: r.hits_total,
            last_hit_at: r.last_hit_at.map(|ts| ts.to_rfc3339()),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRoutesArgs {
    /// Optional group filter (name or ULID).
    pub group: Option<String>,
    /// When `true`, return only routes the caller owns. Defaults to
    /// `false` for admin (sees all) and `true` for non-admin (sees own).
    pub mine: Option<bool>,
    /// Restrict to routes owned by this user. Admin-only — non-admin
    /// callers must use `mine: true` or omit. Takes precedence over
    /// `mine` when both are set.
    pub owner_id: Option<String>,
    /// HTTP method filter (uppercase, e.g. `GET`, or `ANY`).
    pub method: Option<String>,
    /// `*`-glob over the route's defined path (e.g. `/v1/*`).
    pub path_pattern: Option<String>,
    /// Lower bound on `last_hit_at`. Duration suffix (`5m`, `1h`,
    /// `2d`, `30s`) or RFC 3339 timestamp.
    pub since: Option<String>,
    /// Upper bound on `last_hit_at`.
    pub until: Option<String>,
    /// Free-text needle (case-insensitive substring) against path
    /// and methods.
    pub q: Option<String>,
    /// Sort column: `created_at` (default), `last_hit_at`, `hits_total`.
    pub sort: Option<String>,
    /// Sort direction: `asc` or `desc`. Default `desc`.
    pub dir: Option<String>,
    /// Page offset (default 0).
    pub offset: Option<u64>,
    /// Page size (default 50, max 200).
    pub limit: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRoutesResult {
    pub routes: Vec<RouteRecord>,
    /// Total matches after filters, before pagination.
    pub total: u64,
    /// Pass back as `offset` to fetch the next page; absent when the
    /// returned page reached the end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowRouteArgs {
    /// Route slug `{group}/{number}` (e.g. `stripe-mock/7`).
    pub route: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowRouteSourceResult {
    pub slug: String,
    pub language: String,
    /// Original handler source as submitted by the caller. `None` for
    /// pre-compiled `wasm` uploads (no source ever existed in the
    /// host) and for records that pre-date source storage.
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CreateRouteArgs {
    /// Group to add the route to (name or ULID). When omitted, the
    /// host creates an implicit single-route group.
    pub group: Option<String>,
    /// HTTP methods to match. Use `["ANY"]` to match every method.
    pub methods: Vec<String>,
    /// Path pattern. May contain `{param}` segments.
    pub path: String,
    /// Source language. `typescript` or `wasm`.
    pub language: String,
    /// Required when `language: "wasm"`. Pre-compiled component
    /// bytes, base64-encoded.
    pub compiled_wasm_b64: Option<String>,
    /// Bindings version the upload was built against. Defaults to
    /// the host's supported version.
    pub bindings_version: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateRouteArgs {
    /// Route slug `{group}/{number}`.
    pub route: String,
    /// Replace the method list. Omit to leave it unchanged.
    pub methods: Option<Vec<String>>,
    /// Replace the path pattern. Omit to leave it unchanged.
    pub path: Option<String>,
    /// Replace the compiled wasm. Base64-encoded component bytes.
    /// Source-based updates go through REST / `wm routes update
    /// --source-file` — MCP stays wasm-only, matching `create_route`.
    pub compiled_wasm_b64: Option<String>,
    /// Bindings version of the new wasm. Required when
    /// `compiled_wasm_b64` is set.
    pub bindings_version: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DeleteRouteArgs {
    /// Route slug `{group}/{number}`.
    pub route: String,
    /// Required guard. Must be `true`.
    pub confirm: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DeleteRouteResult {
    pub deleted: bool,
}

fn parse_slug(slug: &str) -> Result<(String, u32), ErrorData> {
    let (group, num) = slug
        .rsplit_once('/')
        .ok_or_else(|| validation(format!("expected `{{group}}/{{n}}` slug, got: {slug}")))?;
    let n = num
        .parse::<u32>()
        .map_err(|_| validation(format!("not a valid route number: {num}")))?;
    Ok((group.to_string(), n))
}

#[tool_router(router = routes_router, vis = "pub(crate)")]
impl WmMcpServer {
    #[tool(
        name = "list_routes",
        description = "List routes with optional filters / sort / pagination. Filter by `group`, owner (`mine: true` or admin-only `owner_id`), `method`, `path_pattern` glob, `since`/`until` against `last_hit_at`, free-text `q`. Sort by `created_at` (default), `last_hit_at`, or `hits_total`. Paginate with `offset` + `limit` (default 50, max 200). Response carries `total` + `next_offset`. Non-admin always sees only their own routes."
    )]
    pub async fn list_routes(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListRoutesArgs>,
    ) -> Result<Json<ListRoutesResult>, ErrorData> {
        let auth = auth_from(&parts)?;

        // Parse + validate everything up-front so a bad filter fails
        // before we touch storage.
        let (offset, limit) = parse_pagination(args.offset, args.limit)
            .map_err(crate::mcp::error::map_filter_error)?;
        let dir = SortDir::parse(args.dir.as_deref(), SortDir::Desc)
            .map_err(crate::mcp::error::map_filter_error)?;
        let sort_key =
            parse_route_sort(args.sort.as_deref()).map_err(crate::mcp::error::map_filter_error)?;
        let method = args
            .method
            .as_deref()
            .map(validate_method)
            .transpose()
            .map_err(crate::mcp::error::map_filter_error)?;
        let now = chrono::Utc::now();
        let since = args
            .since
            .as_deref()
            .map(|s| parse_since(s, now))
            .transpose()
            .map_err(crate::mcp::error::map_filter_error)?;
        let until = args
            .until
            .as_deref()
            .map(|s| parse_since(s, now))
            .transpose()
            .map_err(crate::mcp::error::map_filter_error)?;

        // Owner scoping: admins choose any owner / all-owners / mine;
        // non-admins are pinned to themselves regardless of `mine`.
        let owner_filter: Option<String> = if auth.is_admin {
            if let Some(o) = args.owner_id.clone() {
                Some(o)
            } else if args.mine.unwrap_or(false) {
                Some(auth.user_id.clone())
            } else {
                None
            }
        } else {
            if args.owner_id.is_some() {
                return Err(crate::mcp::error::map_filter_error(
                    FilterParseError::OwnerNonAdmin,
                ));
            }
            Some(auth.user_id.clone())
        };

        let all = self
            .state
            .routes()
            .registry()
            .list_routes()
            .map_err(map_registry_error)?;

        let mut filtered: Vec<Route> = all
            .into_iter()
            .filter(|r| match owner_filter.as_deref() {
                Some(o) => r.owner_id == o,
                None => true,
            })
            .filter(|r| match args.group.as_deref() {
                Some(g) => r.group_name == g || r.group_id == g,
                None => true,
            })
            .filter(|r| match method.as_deref() {
                Some(m) => r.methods.iter().any(|rm| rm == m),
                None => true,
            })
            .filter(|r| match args.path_pattern.as_deref() {
                Some(p) => glob_match(p, &r.path),
                None => true,
            })
            .filter(|r| route_matches_since_until(r, since, until))
            .filter(|r| match args.q.as_deref() {
                Some(needle) => route_matches_q(r, needle),
                None => true,
            })
            .collect();

        sort_routes(&mut filtered, sort_key, dir);

        let (page, total, next_offset) = slice_for_page(&filtered, offset, limit);
        Ok(Json(ListRoutesResult {
            routes: page.iter().map(RouteRecord::from).collect(),
            total,
            next_offset,
        }))
    }

    #[tool(
        name = "show_route",
        description = "Show full details of a route by `{group}/{n}` slug. Owner-or-admin only."
    )]
    pub async fn show_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ShowRouteArgs>,
    ) -> Result<Json<RouteRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_slug(&args.route)?;
        let route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        Ok(Json(RouteRecord::from(&route)))
    }

    #[tool(
        name = "show_route_source",
        description = "Return the original handler source the route was created from. `source` is null for routes uploaded as pre-compiled `wasm` (no source ever existed). Owner-or-admin."
    )]
    pub async fn show_route_source(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ShowRouteArgs>,
    ) -> Result<Json<ShowRouteSourceResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_slug(&args.route)?;
        let route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        Ok(Json(ShowRouteSourceResult {
            slug: render_slug(&route.group_name, route.number),
            language: route.language,
            source: route.source,
        }))
    }

    #[tool(
        name = "create_route",
        description = "Create a new route. Slice 10 supports pre-compiled wasm uploads only (`language: \"wasm\"` with `compiled_wasm_b64`). Source-based TypeScript creation is intentionally CLI/REST-side for now (the agent flow is to author with files, not inline strings)."
    )]
    pub async fn create_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CreateRouteArgs>,
    ) -> Result<Json<RouteRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        if args.language != "wasm" {
            return Err(validation(
                "MCP create_route currently supports `language: \"wasm\"` only — \
                 use the REST endpoint or `wm routes add --source-file` for \
                 source-based handlers in slice 10",
            ));
        }
        let b64 = args.compiled_wasm_b64.ok_or_else(|| {
            validation("`compiled_wasm_b64` is required when language is \"wasm\"")
        })?;
        let bytes = B64
            .decode(&b64)
            .map_err(|e| validation(format!("compiled_wasm_b64 is not valid base64: {e}")))?;
        let route = self
            .state
            .routes()
            .registry()
            .create_route(NewRoute {
                group: args.group,
                methods: args.methods,
                path: args.path,
                language: args.language,
                bindings_version: args
                    .bindings_version
                    .unwrap_or_else(|| SUPPORTED_BINDINGS_VERSION.into()),
                compiled_wasm: bytes,
                // MCP is wasm-only on create, so no source ever flows
                // through this surface.
                source: None,
                owner_id: auth.user_id,
            })
            .map_err(map_registry_error)?;
        // Keep the in-memory route table coherent.
        self.state.routes().refresh_after_create(route.clone());
        Ok(Json(RouteRecord::from(&route)))
    }

    #[tool(
        name = "update_route",
        description = "Update a route's mutable fields by `{group}/{n}` slug. Owner-or-admin only. Pass at least one of `methods`, `path`, or `compiled_wasm_b64`. MCP stays wasm-only for the artifact (source-based updates use REST or `wm routes update --source-file`)."
    )]
    pub async fn update_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateRouteArgs>,
    ) -> Result<Json<RouteRecord>, ErrorData> {
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_slug(&args.route)?;
        let existing = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        if !auth.is_admin && existing.owner_id != auth.user_id {
            return Err(forbidden(
                "only the route's owner or an admin may update it",
            ));
        }

        let (compiled_wasm, language, bindings_version) = match args.compiled_wasm_b64 {
            Some(b64) => {
                let bytes = B64.decode(&b64).map_err(|e| {
                    validation(format!("compiled_wasm_b64 is not valid base64: {e}"))
                })?;
                let bv = args
                    .bindings_version
                    .unwrap_or_else(|| SUPPORTED_BINDINGS_VERSION.into());
                if bv != SUPPORTED_BINDINGS_VERSION {
                    return Err(validation(format!(
                        "bindings_version {bv:?} is not supported (expected {:?})",
                        SUPPORTED_BINDINGS_VERSION
                    )));
                }
                wasmtime::component::Component::from_binary(self.state.runtime().engine(), &bytes)
                    .map_err(|e| validation(format!("component validation: {e}")))?;
                (Some(bytes), Some("wasm".to_string()), Some(bv))
            }
            None => (None, None, None),
        };

        if args.methods.is_none() && args.path.is_none() && compiled_wasm.is_none() {
            return Err(validation(
                "update_route needs at least one of `methods`, `path`, `compiled_wasm_b64`",
            ));
        }

        let updated = self
            .state
            .routes()
            .registry()
            .update_route(
                &group_ref,
                number,
                PatchRoute {
                    methods: args.methods,
                    path: args.path,
                    language,
                    bindings_version,
                    // MCP is wasm-only on update — a wasm swap always
                    // clears any prior source (Some(None)); a no-op
                    // artifact keeps source alone (None).
                    source: if compiled_wasm.is_some() {
                        Some(None)
                    } else {
                        None
                    },
                    compiled_wasm,
                },
            )
            .map_err(map_registry_error)?;
        self.state.routes().refresh_after_update(updated.clone());
        Ok(Json(RouteRecord::from(&updated)))
    }

    #[tool(
        name = "delete_route",
        description = "Delete a single route by `{group}/{n}` slug. Cascades the route's per-route kv state. `confirm` must be `true`."
    )]
    pub async fn delete_route(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<DeleteRouteArgs>,
    ) -> Result<Json<DeleteRouteResult>, ErrorData> {
        if !args.confirm {
            return Err(validation(
                "delete_route requires `confirm: true` — set it explicitly to proceed",
            ));
        }
        let auth = auth_from(&parts)?;
        let (group_ref, number) = parse_slug(&args.route)?;
        let route = ensure_route_owner_or_admin(&self.state, &auth, &group_ref, number)?;
        if !auth.is_admin && route.owner_id != auth.user_id {
            // Defensive: ensure_route_owner_or_admin already checks
            // this, but the explicit guard documents intent.
            return Err(forbidden("must be the route's owner or an admin"));
        }
        self.state
            .routes()
            .registry()
            .delete_route(&group_ref, number)
            .map_err(|e| match e {
                crate::registry::RegistryError::NotFound => {
                    crate::mcp::error::not_found("route not found")
                }
                other => map_registry_error(other),
            })?;
        self.state.routes().refresh_after_delete(&route.id);
        Ok(Json(DeleteRouteResult { deleted: true }))
    }
}
