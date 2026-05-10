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
use crate::mcp::context::{auth_from, ensure_route_owner_or_admin};
use crate::mcp::error::{forbidden, map_registry_error, validation};
use crate::mcp::server::WmMcpServer;
use crate::registry::{NewRoute, Route};

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
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRoutesArgs {
    /// Optional group filter (name or ULID).
    pub group: Option<String>,
    /// When `true`, return only routes the caller owns. Defaults to
    /// `false` (admin sees all; non-admin sees their own regardless).
    pub mine: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRoutesResult {
    pub routes: Vec<RouteRecord>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ShowRouteArgs {
    /// Route slug `{group}/{number}` (e.g. `stripe-mock/7`).
    pub route: String,
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
        description = "List routes. Filter by group, by owner (--mine), or both. Non-admin sees only their own; admin sees all unless `mine: true`."
    )]
    pub async fn list_routes(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListRoutesArgs>,
    ) -> Result<Json<ListRoutesResult>, ErrorData> {
        let auth = auth_from(&parts)?;
        let only_mine = args.mine.unwrap_or(!auth.is_admin);
        let mut routes = if only_mine {
            self.state
                .routes()
                .registry()
                .list_routes_by_owner(&auth.user_id)
                .map_err(map_registry_error)?
        } else {
            self.state
                .routes()
                .registry()
                .list_routes()
                .map_err(map_registry_error)?
        };
        if let Some(group_ref) = args.group.as_deref() {
            routes.retain(|r| r.group_id == group_ref || r.group_name == group_ref);
        }
        Ok(Json(ListRoutesResult {
            routes: routes.iter().map(RouteRecord::from).collect(),
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
                owner_id: auth.user_id,
            })
            .map_err(map_registry_error)?;
        // Keep the in-memory route table coherent.
        self.state.routes().refresh_after_create(route.clone());
        Ok(Json(RouteRecord::from(&route)))
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
