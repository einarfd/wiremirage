//! Web UI routes under `/__ui/*` (slice 21).
//!
//! The UI is for inspection + token management + a small set of
//! admin actions — handlers are deliberately *not* the primary place
//! to author mocks (agents and the API are). See `web-ui-design.md`.
//!
//! Templates are embedded with `include_str!` at compile time so
//! `cargo build` produces a single binary. minijinja parses them once
//! at startup; rendering is cheap.
//!
//! Auth on `/__ui/*`: a middleware (`require_session`) redirects
//! unauthenticated browsers to `/__auth/login?next=...`. Inside the
//! handler we reuse the standard `AuthContext` extractor since the
//! middleware has already established a valid session.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use minijinja::Environment;
use minijinja::context;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::{GroupsListQuery, RoutesListQuery, list_groups_core, list_routes_core};
use crate::api_filters::validate_method;
use crate::auth::AuthContext;
use crate::journal::UnmatchedCursor;
use crate::journal_filter::JournalFilter;
use crate::registry::{Group, Route};
use crate::wire::WireBytes;

pub mod auth_redirect;
pub mod csrf;
pub mod static_assets;

/// Process-wide template environment. Built once at startup; cloning
/// is cheap (the inner Arc is shared).
#[derive(Clone)]
pub struct UiTemplates {
    env: Arc<Environment<'static>>,
}

impl UiTemplates {
    pub fn new() -> Self {
        let mut env = Environment::new();
        // One `add_template` call per file. Macro-driven so a new
        // template is one line at the call site — easy to keep in
        // sync as new screens land in subsequent slices.
        macro_rules! tmpl {
            ($name:literal, $path:literal) => {
                env.add_template($name, include_str!($path))
                    .expect("template parses at compile time");
            };
        }
        tmpl!("base.html", "templates/base.html");
        tmpl!("login.html", "templates/login.html");
        tmpl!("home.html", "templates/home.html");
        tmpl!("connect.html", "templates/connect.html");
        tmpl!("groups_list.html", "templates/groups_list.html");
        tmpl!("groups_new.html", "templates/groups_new.html");
        tmpl!("group_detail.html", "templates/group_detail.html");
        tmpl!("routes_list.html", "templates/routes_list.html");
        tmpl!("route_new.html", "templates/route_new.html");
        tmpl!("route_detail.html", "templates/route_detail.html");
        tmpl!("live_journal.html", "templates/live_journal.html");
        tmpl!("journal_entry.html", "templates/journal_entry.html");
        tmpl!("tokens.html", "templates/tokens.html");
        tmpl!("group_state.html", "templates/group_state.html");
        tmpl!("route_state.html", "templates/route_state.html");
        tmpl!("route_dry_run.html", "templates/route_dry_run.html");
        tmpl!("route_source_edit.html", "templates/route_source_edit.html");
        tmpl!("unmatched_list.html", "templates/unmatched_list.html");
        tmpl!("unmatched_detail.html", "templates/unmatched_detail.html");
        tmpl!("not_found.html", "templates/not_found.html");
        tmpl!("oauth_consent.html", "templates/oauth_consent.html");
        tmpl!("placeholder.html", "templates/placeholder.html");
        Self { env: Arc::new(env) }
    }

    pub fn render<S: Serialize>(&self, name: &str, ctx: S) -> Result<String, minijinja::Error> {
        let tmpl = self.env.get_template(name)?;
        tmpl.render(ctx)
    }
}

impl Default for UiTemplates {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `/__ui/*` sub-router. Mounted from `server::router`.
/// Takes state by value so the auth-redirect middleware can hold a
/// clone — the merged router from `server::router` provides the same
/// state to inner handlers separately.
pub fn router(state: AppState) -> Router {
    let authed: Router<AppState> = Router::new()
        .route("/__ui/", get(home))
        .route("/__ui/connect", get(connect))
        .route("/__ui/groups", get(groups_list_page))
        // Register before `/__ui/groups/{group}` — matchit prefers the
        // static segment, but keep them adjacent for clarity.
        .route(
            "/__ui/groups/new",
            get(group_new_form).post(group_new_submit),
        )
        .route("/__ui/groups/{group}", get(group_detail_page))
        .route(
            "/__ui/groups/{group}/refresh",
            axum::routing::post(group_refresh_form),
        )
        .route(
            "/__ui/groups/{group}/edit",
            axum::routing::post(group_edit_form),
        )
        .route(
            "/__ui/groups/{group}/delete",
            axum::routing::post(group_delete_form),
        )
        .route(
            "/__ui/groups/{group}/state",
            get(group_state_page).post(group_state_clear_form),
        )
        .route("/__ui/routes", get(routes_list_page))
        .route(
            "/__ui/routes/new",
            get(route_new_form).post(route_new_submit),
        )
        .route("/__ui/routes/{group}/{number}", get(route_detail_page))
        .route(
            "/__ui/routes/{group}/{number}/delete",
            axum::routing::post(route_delete_form),
        )
        .route(
            "/__ui/routes/{group}/{number}/state",
            get(route_state_page).post(route_state_clear_form),
        )
        .route(
            "/__ui/routes/{group}/{number}/dry-run",
            get(route_dry_run_page).post(route_dry_run_submit),
        )
        .route(
            "/__ui/routes/{group}/{number}/source/edit",
            get(route_source_edit_page).post(route_source_edit_submit),
        )
        .route("/__ui/journal/live", get(live_journal_page))
        .route("/__ui/journal/{group}/{number}", get(journal_entry_page))
        .route("/__ui/unmatched", get(unmatched_index_page))
        .route("/__ui/unmatched/{number}", get(unmatched_detail_page))
        .route("/__ui/me/tokens", get(tokens_page).post(create_token_form))
        .route(
            "/__ui/me/tokens/{name}/revoke",
            axum::routing::post(revoke_token_form),
        )
        .route(
            "/__ui/me/tokens/{name}/rename",
            axum::routing::post(rename_token_form),
        )
        .route(
            "/__ui/me/tokens/oauth/{client_id}/revoke",
            axum::routing::post(revoke_oauth_grant_form),
        )
        .route("/__ui/settings", get(stub_settings))
        .route("/__ui/admin/health", get(stub_admin_health))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf::csrf_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_redirect::require_session,
        ));

    let public: Router<AppState> = Router::new()
        // Static assets are deliberately *not* behind auth — CSS for
        // the login page would chicken-and-egg otherwise.
        .route("/__ui/static/{*path}", get(static_assets::serve));

    authed.merge(public).with_state(state)
}

// -- Handlers ---------------------------------------------------------------

/// "Connect an agent" — MCP onboarding. Shows the live MCP endpoint
/// (derived from the request's forwarded headers, so it matches the
/// public origin) and paste-ready client configs. No forms; the token
/// is a placeholder with a link to the tokens page.
async fn connect(State(state): State<AppState>, auth: AuthContext, headers: HeaderMap) -> Response {
    let base = crate::auth_api::public_base_url(&headers, state.trust_forwarded_headers());
    let mcp_url = format!("{base}/__api/mcp");
    render(
        &state,
        "connect.html",
        context! {
            page_title => "Connect an agent",
            user => UserBadge::from(&auth),
            base_url => base,
            mcp_url => mcp_url,
            apex_host => state.apex_host(),
        },
    )
}

async fn home(State(state): State<AppState>, auth: AuthContext) -> Response {
    // List groups visible to the caller. Admin sees all; non-admin
    // sees their own. Same rule as the REST /__api/groups endpoint.
    let groups = if auth.is_admin {
        state.routes().registry().list_groups()
    } else {
        state
            .routes()
            .registry()
            .list_groups_by_owner(&auth.user_id)
    };
    let groups = match groups {
        Ok(g) => g,
        Err(e) => return ui_error_500(&state, &auth, format!("registry: {e}")),
    };

    // Convert into a minimal struct minijinja can iterate over —
    // template needs name, last_activity, route_count.
    let all_routes = state.routes().registry().list_routes().unwrap_or_default();
    let mut visible: Vec<HomeGroupRow> = groups
        .into_iter()
        .map(|g| {
            let route_count = all_routes.iter().filter(|r| r.group_id == g.id).count();
            HomeGroupRow {
                name: g.name,
                last_activity: g.last_activity_at.map(|t| t.to_rfc3339()),
                route_count,
                ttl_seconds: g.ttl_seconds,
            }
        })
        .collect();
    visible.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

    render(
        &state,
        "home.html",
        context! {
            page_title => "Home",
            user => UserBadge::from(&auth),
            groups => visible,
        },
    )
}

#[derive(Serialize)]
struct HomeGroupRow {
    name: String,
    last_activity: Option<String>,
    route_count: usize,
    ttl_seconds: u64,
}

// -- /__ui/groups -----------------------------------------------------------
//
// Mirrors `GET /__api/groups` from slice 18, with two UI affordances on top:
//
// * `owner_scope=mine|everyone` instead of raw `owner_id`. Admins flip
//   between "just my groups" and "all owners"; non-admins can't see other
//   owners' groups anyway so the dropdown is hidden in the template.
// * Sort-toggle column headers — clicking a column flips asc/desc when
//   it's already the active sort, otherwise resets to that column's
//   default direction.

const UI_PAGE_LIMIT: u64 = 25;

#[derive(Debug, Deserialize, Default)]
struct UiGroupsQuery {
    q: Option<String>,
    name_prefix: Option<String>,
    implicit: Option<String>,
    /// "mine" (default) or "everyone" — admin-only on the form.
    owner_scope: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    offset: Option<u64>,
}

async fn groups_list_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<UiGroupsQuery>,
) -> Response {
    let owner_scope = effective_owner_scope(&auth, q.owner_scope.as_deref());
    let implicit = match q.implicit.as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    let core_q = GroupsListQuery {
        owner_id: owner_id_for(&auth, &owner_scope),
        name_prefix: nonempty(q.name_prefix.as_deref()),
        q: nonempty(q.q.as_deref()),
        since: None,
        until: None,
        implicit,
        sort: q.sort.clone(),
        dir: q.dir.clone(),
        offset: q.offset,
        limit: Some(UI_PAGE_LIMIT),
    };

    let paged = match list_groups_core(&state, &auth, &core_q) {
        Ok(p) => p,
        Err(e) => return ui_error_400(&state, &auth, format!("filter error: {}", e.message())),
    };

    let owner_names = resolve_owner_names(&state, paged.groups.iter().map(|g| g.owner_id.as_str()));
    let all_routes = state.routes().registry().list_routes().unwrap_or_default();
    let rows: Vec<GroupsListRow> = paged
        .groups
        .iter()
        .map(|g| GroupsListRow::from_group(g, &owner_names, &all_routes))
        .collect();

    let sort = q.sort.clone().unwrap_or_else(|| "last_activity_at".into());
    let dir = q.dir.clone().unwrap_or_else(|| "desc".into());
    let filters = GroupsFilterState {
        q: q.q.clone().unwrap_or_default(),
        implicit: q.implicit.clone().unwrap_or_default(),
        owner_scope: owner_scope.clone(),
        sort: sort.clone(),
        dir: dir.clone(),
        dir_arrow: arrow_for(&dir),
        any_active: q.q.is_some()
            || q.name_prefix.is_some()
            || q.implicit.is_some()
            || q.owner_scope.as_deref() == Some("everyone"),
    };
    let sort_links = GroupsSortLinks::build(&q, &sort, &dir);
    let pagination = pagination_for(
        "/__ui/groups",
        &q.serialize_for_paging(),
        q.offset,
        paged.total,
        UI_PAGE_LIMIT,
        paged.next_offset,
    );
    let showing = rows.len() as u64;

    render(
        &state,
        "groups_list.html",
        context! {
            page_title => "Groups",
            user => UserBadge::from(&auth),
            groups => rows,
            total => paged.total,
            showing => showing,
            filters => filters,
            sort_links => sort_links,
            pagination => pagination,
        },
    )
}

#[derive(Serialize)]
struct GroupsListRow {
    name: String,
    implicit: bool,
    owner_name: String,
    route_count: usize,
    ttl_seconds: u64,
    created_at: String,
    last_activity_at: Option<String>,
}

impl GroupsListRow {
    fn from_group(g: &Group, owner_names: &HashMap<String, String>, routes: &[Route]) -> Self {
        let route_count = routes.iter().filter(|r| r.group_id == g.id).count();
        Self {
            name: g.name.clone(),
            implicit: g.implicit,
            owner_name: owner_names
                .get(&g.owner_id)
                .cloned()
                .unwrap_or_else(|| short_id(&g.owner_id)),
            route_count,
            ttl_seconds: g.ttl_seconds,
            created_at: g.created_at.to_rfc3339(),
            last_activity_at: g.last_activity_at.map(|t| t.to_rfc3339()),
        }
    }
}

#[derive(Serialize)]
struct GroupsFilterState {
    q: String,
    implicit: String,
    owner_scope: String,
    sort: String,
    dir: String,
    dir_arrow: &'static str,
    any_active: bool,
}

#[derive(Serialize)]
struct GroupsSortLinks {
    name: String,
    created_at: String,
    last_activity_at: String,
}

impl GroupsSortLinks {
    fn build(q: &UiGroupsQuery, current_sort: &str, current_dir: &str) -> Self {
        let base = q.serialize_for_paging();
        let mk = |col: &str, default_dir: &str| -> String {
            let dir = if current_sort == col {
                flip(current_dir)
            } else {
                default_dir
            };
            let mut parts = base.clone();
            parts.push(("sort".to_string(), col.to_string()));
            parts.push(("dir".to_string(), dir.to_string()));
            format!("/__ui/groups?{}", encode_query(&parts))
        };
        Self {
            name: mk("name", "asc"),
            created_at: mk("created_at", "desc"),
            last_activity_at: mk("last_activity_at", "desc"),
        }
    }
}

impl UiGroupsQuery {
    /// Echo every non-empty filter as a `(name, value)` pair so we can
    /// rebuild URLs (paging, sort-toggle) without losing user input.
    /// Sort/dir/offset deliberately excluded — the link builder sets
    /// those.
    fn serialize_for_paging(&self) -> Vec<(String, String)> {
        let mut parts = Vec::new();
        if let Some(v) = nonempty(self.q.as_deref()) {
            parts.push(("q".to_string(), v));
        }
        if let Some(v) = nonempty(self.name_prefix.as_deref()) {
            parts.push(("name_prefix".to_string(), v));
        }
        if let Some(v) = nonempty(self.implicit.as_deref()) {
            parts.push(("implicit".to_string(), v));
        }
        if let Some(v) = nonempty(self.owner_scope.as_deref()) {
            parts.push(("owner_scope".to_string(), v));
        }
        parts
    }
}

// -- /__ui/routes -----------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct UiRoutesQuery {
    group: Option<String>,
    method: Option<String>,
    path_pattern: Option<String>,
    owner_scope: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    offset: Option<u64>,
}

async fn routes_list_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<UiRoutesQuery>,
) -> Response {
    let owner_scope = effective_owner_scope(&auth, q.owner_scope.as_deref());
    let core_q = RoutesListQuery {
        group: nonempty(q.group.as_deref()),
        owner_id: owner_id_for(&auth, &owner_scope),
        method: nonempty(q.method.as_deref()),
        path_pattern: nonempty(q.path_pattern.as_deref()),
        since: None,
        until: None,
        q: None,
        sort: q.sort.clone(),
        dir: q.dir.clone(),
        offset: q.offset,
        limit: Some(UI_PAGE_LIMIT),
    };

    let paged = match list_routes_core(&state, &auth, &core_q) {
        Ok(p) => p,
        Err(e) => return ui_error_400(&state, &auth, format!("filter error: {}", e.message())),
    };

    let owner_names = resolve_owner_names(&state, paged.routes.iter().map(|r| r.owner_id.as_str()));
    let rows: Vec<RoutesListRow> = paged
        .routes
        .iter()
        .map(|r| RoutesListRow::from_route(r, &owner_names))
        .collect();

    let sort = q.sort.clone().unwrap_or_else(|| "last_hit_at".into());
    let dir = q.dir.clone().unwrap_or_else(|| "desc".into());
    let filters = RoutesFilterState {
        group: q.group.clone().unwrap_or_default(),
        method: q.method.clone().unwrap_or_default(),
        path_pattern: q.path_pattern.clone().unwrap_or_default(),
        owner_scope: owner_scope.clone(),
        sort: sort.clone(),
        dir: dir.clone(),
        dir_arrow: arrow_for(&dir),
        any_active: q.group.is_some()
            || q.method.is_some()
            || q.path_pattern.is_some()
            || q.owner_scope.as_deref() == Some("everyone"),
    };
    let sort_links = RoutesSortLinks::build(&q, &sort, &dir);
    let pagination = pagination_for(
        "/__ui/routes",
        &q.serialize_for_paging(),
        q.offset,
        paged.total,
        UI_PAGE_LIMIT,
        paged.next_offset,
    );
    let showing = rows.len() as u64;
    // Pre-compute the group dropdown options. Admins see all groups
    // (matching the default `owner_scope=everyone`), non-admin sees
    // only their owned groups. Implicit single-route groups are
    // filtered out — they're auto-generated and not interesting to
    // filter by.
    let registry = state.routes().registry();
    let mut available_groups: Vec<String> = if auth.is_admin {
        registry.list_groups().unwrap_or_default()
    } else {
        registry
            .list_groups_by_owner(&auth.user_id)
            .unwrap_or_default()
    }
    .into_iter()
    .filter(|g| !g.implicit)
    .map(|g| g.name)
    .collect();
    available_groups.sort();

    render(
        &state,
        "routes_list.html",
        context! {
            page_title => "Routes",
            user => UserBadge::from(&auth),
            routes => rows,
            total => paged.total,
            showing => showing,
            filters => filters,
            sort_links => sort_links,
            pagination => pagination,
            methods => ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
            available_groups => available_groups,
        },
    )
}

#[derive(Serialize)]
struct RoutesListRow {
    number: u32,
    group_name: String,
    methods: String,
    path: String,
    language: String,
    owner_name: String,
    hits_total: u64,
    last_hit_at: Option<String>,
}

impl RoutesListRow {
    fn from_route(r: &Route, owner_names: &HashMap<String, String>) -> Self {
        Self {
            number: r.number,
            group_name: r.group_name.clone(),
            methods: r.methods.join(", "),
            path: r.path.clone(),
            language: r.language.clone(),
            owner_name: owner_names
                .get(&r.owner_id)
                .cloned()
                .unwrap_or_else(|| short_id(&r.owner_id)),
            hits_total: r.hits_total,
            last_hit_at: r.last_hit_at.map(|t| t.to_rfc3339()),
        }
    }
}

#[derive(Serialize)]
struct RoutesFilterState {
    group: String,
    method: String,
    path_pattern: String,
    owner_scope: String,
    sort: String,
    dir: String,
    dir_arrow: &'static str,
    any_active: bool,
}

#[derive(Serialize)]
struct RoutesSortLinks {
    last_hit_at: String,
    hits_total: String,
}

impl RoutesSortLinks {
    fn build(q: &UiRoutesQuery, current_sort: &str, current_dir: &str) -> Self {
        let base = q.serialize_for_paging();
        let mk = |col: &str, default_dir: &str| -> String {
            let dir = if current_sort == col {
                flip(current_dir)
            } else {
                default_dir
            };
            let mut parts = base.clone();
            parts.push(("sort".to_string(), col.to_string()));
            parts.push(("dir".to_string(), dir.to_string()));
            format!("/__ui/routes?{}", encode_query(&parts))
        };
        Self {
            last_hit_at: mk("last_hit_at", "desc"),
            hits_total: mk("hits_total", "desc"),
        }
    }
}

impl UiRoutesQuery {
    fn serialize_for_paging(&self) -> Vec<(String, String)> {
        let mut parts = Vec::new();
        if let Some(v) = nonempty(self.group.as_deref()) {
            parts.push(("group".to_string(), v));
        }
        if let Some(v) = nonempty(self.method.as_deref()) {
            parts.push(("method".to_string(), v));
        }
        if let Some(v) = nonempty(self.path_pattern.as_deref()) {
            parts.push(("path_pattern".to_string(), v));
        }
        if let Some(v) = nonempty(self.owner_scope.as_deref()) {
            parts.push(("owner_scope".to_string(), v));
        }
        parts
    }
}

// -- List-page shared helpers ----------------------------------------------

/// "mine" is the safe default for non-admins (the core fn rejects any
/// other choice anyway). Admins default to "everyone" — the home page
/// already shows them their own groups as a preview, so the list pages
/// open onto the full host view.
fn effective_owner_scope(auth: &AuthContext, raw: Option<&str>) -> String {
    if !auth.is_admin {
        return "mine".into();
    }
    match raw {
        Some("mine") => "mine".into(),
        _ => "everyone".into(),
    }
}

/// Translate the UI's `owner_scope` enum into the core fn's
/// `owner_id` parameter. Non-admin callers always get `None` — the
/// core fn then scopes to the caller's user_id internally.
fn owner_id_for(auth: &AuthContext, owner_scope: &str) -> Option<String> {
    if !auth.is_admin {
        return None;
    }
    match owner_scope {
        "mine" => Some(auth.user_id.clone()),
        _ => None,
    }
}

fn nonempty(s: Option<&str>) -> Option<String> {
    match s {
        Some(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn arrow_for(dir: &str) -> &'static str {
    if dir == "asc" { "↑" } else { "↓" }
}

fn flip(dir: &str) -> &'static str {
    if dir == "asc" { "desc" } else { "asc" }
}

fn short_id(id: &str) -> String {
    // ULIDs are 26 chars; the last 8 are usually enough to disambiguate
    // for display while staying compact. Falls back to the full string
    // for non-ULID inputs.
    if id.len() > 8 {
        format!("…{}", &id[id.len() - 8..])
    } else {
        id.to_string()
    }
}

fn resolve_owner_names<'a, I: Iterator<Item = &'a str>>(
    state: &AppState,
    ids: I,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        match state.auth().get_user_by_id(id) {
            Ok(u) => {
                out.insert(id.to_string(), u.name);
            }
            Err(_) => {
                out.insert(id.to_string(), short_id(id));
            }
        }
    }
    out
}

fn encode_query(parts: &[(String, String)]) -> String {
    parts
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[derive(Serialize)]
struct PaginationCtx {
    page: u64,
    page_count: u64,
    prev_url: Option<String>,
    next_url: Option<String>,
}

fn pagination_for(
    base_path: &str,
    base_params: &[(String, String)],
    offset: Option<u64>,
    total: u64,
    limit: u64,
    next_offset: Option<u64>,
) -> PaginationCtx {
    let cur = offset.unwrap_or(0);
    let page = cur / limit + 1;
    let page_count = if total == 0 { 1 } else { total.div_ceil(limit) };

    let mk_url = |off: u64| {
        let mut parts: Vec<(String, String)> = base_params.to_vec();
        if off > 0 {
            parts.push(("offset".into(), off.to_string()));
        }
        if parts.is_empty() {
            base_path.to_string()
        } else {
            format!("{}?{}", base_path, encode_query(&parts))
        }
    };

    let prev_url = if cur >= limit {
        Some(mk_url(cur - limit))
    } else if cur > 0 {
        Some(mk_url(0))
    } else {
        None
    };
    let next_url = next_offset.map(mk_url);

    PaginationCtx {
        page,
        page_count,
        prev_url,
        next_url,
    }
}

fn ui_error_400(state: &AppState, auth: &AuthContext, msg: String) -> Response {
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Bad filter",
            api_hint => msg,
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

#[derive(Serialize)]
pub(crate) struct UserBadge {
    pub name: String,
    pub is_admin: bool,
}

impl UserBadge {
    pub fn from(auth: &AuthContext) -> Self {
        Self {
            name: auth.user_name.clone(),
            is_admin: auth.is_admin,
        }
    }
}

// -- /__ui/groups/{group} ---------------------------------------------------
//
// Detail page for a single group. Renders metadata, the group's
// routes (link-through to per-route detail), and a "Manage from CLI"
// help block. Authed actions (refresh / edit / delete) wait for a
// later slice that brings CSRF middleware online.

async fn group_detail_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Response {
    let group = match state.routes().registry().read_group_by_ref(&group_ref) {
        Ok(g) => g,
        Err(_) => return ui_not_found(&state, &auth, &format!("Group {group_ref}")),
    };
    if !auth.is_admin && group.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }

    let all_routes = state.routes().registry().list_routes().unwrap_or_default();
    let mut routes_in_group: Vec<&Route> = all_routes
        .iter()
        .filter(|r| r.group_id == group.id)
        .collect();
    routes_in_group.sort_by_key(|r| r.number);

    let owner_names = resolve_owner_names(&state, std::iter::once(group.owner_id.as_str()));
    let routes_view: Vec<GroupDetailRouteRow> = routes_in_group
        .iter()
        .map(|r| GroupDetailRouteRow {
            number: r.number,
            methods: r.methods.join(", "),
            path: r.path.clone(),
            hits_total: r.hits_total,
            last_hit_at: r.last_hit_at.map(|t| t.to_rfc3339()),
        })
        .collect();
    let group_view = GroupDetailGroup {
        id: group.id.clone(),
        name: group.name.clone(),
        implicit: group.implicit,
        owner_name: owner_names
            .get(&group.owner_id)
            .cloned()
            .unwrap_or_else(|| short_id(&group.owner_id)),
        ttl_seconds: group.ttl_seconds,
        sliding_ttl: group.sliding_ttl,
        last_activity_at: group.last_activity_at.map(|t| t.to_rfc3339()),
        created_at: group.created_at.to_rfc3339(),
    };

    // Pre-fetch the most recent few journal entries for this group
    // so the live pane has content on first paint; the EventSource
    // in the template then keeps it current.
    let recent_entries: Vec<LiveJournalRow> =
        fetch_recent_for_group(&state, &group.id, &group.name, 10)
            .into_iter()
            .map(|(r, name)| LiveJournalRow::from_record(&r, &name))
            .collect();
    let sse_url = format!(
        "/__api/journal/tail?group={}",
        urlencoding::encode(&group.name)
    );

    render(
        &state,
        "group_detail.html",
        context! {
            page_title => group.name,
            user => UserBadge::from(&auth),
            group => group_view,
            routes => routes_view,
            route_count => routes_in_group.len(),
            recent_entries => recent_entries,
            sse_url => sse_url,
        },
    )
}

#[derive(Serialize)]
struct GroupDetailGroup {
    id: String,
    name: String,
    implicit: bool,
    owner_name: String,
    ttl_seconds: u64,
    sliding_ttl: bool,
    last_activity_at: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
struct GroupDetailRouteRow {
    number: u32,
    methods: String,
    path: String,
    hits_total: u64,
    last_hit_at: Option<String>,
}

// -- Group action handlers (slice 26) ---------------------------------------
//
// POST endpoints that the buttons + edit form on the group-detail
// page submit to. Owner-or-admin gate, CSRF middleware handles the
// `_csrf` form-field check, then we mutate via the registry and
// 303 back to a sensible landing page (the detail page on
// edit/refresh, the listing on delete).

#[derive(serde::Deserialize)]
struct EditGroupForm {
    /// New name (DNS label). Empty/missing/unchanged leaves the name as-is;
    /// a change rewrites the group's subdomain (ADR-0030).
    name: Option<String>,
    /// New TTL in seconds. Empty/missing leaves the existing value.
    ttl_seconds: Option<String>,
    /// "on" when checked; absent when unchecked (HTML form quirk).
    sliding_ttl: Option<String>,
    #[serde(rename = "_csrf")]
    _csrf: Option<String>,
}

async fn group_refresh_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    let group = match resolve_owned_group(&state, &auth, &group_ref) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    if let Err(e) = state.routes().registry().refresh_group(&group.id) {
        return ui_error_500(&state, &auth, format!("refresh: {e}"));
    }
    axum::response::Redirect::to(&format!("/__ui/groups/{}", group.name)).into_response()
}

async fn group_edit_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    axum::Form(form): axum::Form<EditGroupForm>,
) -> Response {
    let group = match resolve_owned_group(&state, &auth, &group_ref) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    // Rename first (rewrites the by-name index + route slugs + the served
    // subdomain), mirroring MCP `update_group`. Skip when unchanged/blank.
    let mut final_name = group.name.clone();
    if let Some(new_name) = form
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != group.name)
    {
        match state.routes().registry().rename_group(&group.id, new_name) {
            Ok(renamed) => {
                state
                    .routes()
                    .refresh_after_group_rename(&group.id, &renamed.name);
                final_name = renamed.name;
            }
            Err(e) => return ui_error_400_text(&state, &auth, &format!("rename: {e}")),
        }
    }
    let ttl_seconds = match form.ttl_seconds.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(s) => match s.parse::<u64>() {
            Ok(v) if v > 0 => Some(v),
            _ => {
                return ui_error_400_text(&state, &auth, "TTL seconds must be a positive integer.");
            }
        },
    };
    // HTML checkboxes only submit the field when checked; an
    // unchecked box means "sliding off". So if the form arrived
    // without ttl_seconds and without sliding_ttl, treat that as
    // "no change". If sliding_ttl was explicitly present (or
    // explicitly absent on a submit), wire the new value through.
    let sliding = form.sliding_ttl.as_deref().map(|v| {
        let v = v.trim();
        v == "on" || v == "true" || v == "1"
    });
    // Always submit sliding_ttl change because the form *not having*
    // the field means the checkbox was unchecked — but only treat the
    // POST as setting it when the form mentioned the field at all.
    // To distinguish, the template renders a hidden marker
    // `sliding_ttl_marker=1` so we know the form was for editing.
    let sliding_explicit = form.sliding_ttl.is_some() || ttl_seconds.is_some();
    let sliding_to_set = if sliding_explicit {
        Some(sliding.unwrap_or(false))
    } else {
        None
    };

    if let Err(e) = state
        .routes()
        .registry()
        .patch_group(&group.id, ttl_seconds, sliding_to_set)
    {
        return ui_error_500(&state, &auth, format!("edit: {e}"));
    }
    axum::response::Redirect::to(&format!("/__ui/groups/{final_name}")).into_response()
}

async fn group_delete_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    let group = match resolve_owned_group(&state, &auth, &group_ref) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    if let Err(e) = state.routes().registry().cascade_delete_group(&group.id) {
        return ui_error_500(&state, &auth, format!("delete: {e}"));
    }
    state.routes().refresh_after_group_cascade(&group.id);
    axum::response::Redirect::to("/__ui/groups").into_response()
}

/// Look up `group_ref` and confirm the caller can manage it. The
/// detail page uses a 403/404 split; lifecycle endpoints use the same
/// rule. Returns `Box<Response>` on rejection so the `Result`'s Err
/// variant stays small (clippy's `result_large_err` lint).
fn resolve_owned_group(
    state: &AppState,
    auth: &AuthContext,
    group_ref: &str,
) -> Result<crate::registry::Group, Box<Response>> {
    let group = state
        .routes()
        .registry()
        .read_group_by_ref(group_ref)
        .map_err(|_| Box::new(ui_not_found(state, auth, &format!("Group {group_ref}"))))?;
    if !auth.is_admin && group.owner_id != auth.user_id {
        return Err(Box::new(forbidden_page(state, auth)));
    }
    Ok(group)
}

fn ui_error_400_text(state: &AppState, auth: &AuthContext, msg: &str) -> Response {
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Bad request",
            api_hint => msg,
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

// -- /__ui/routes/{group}/{number} ------------------------------------------
//
// Detail page for a single route. Metadata + a short tail of recent
// journal entries for this route (read from the group's journal,
// filtered by route_id). Source viewing waits for the CodeMirror
// slice; authed actions wait for CSRF.

async fn route_detail_page(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    Path((group_ref, number)): Path<(String, u32)>,
) -> Response {
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => {
            return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}"));
        }
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }

    let owner_names = resolve_owner_names(&state, std::iter::once(route.owner_id.as_str()));
    let owner_name = owner_names
        .get(&route.owner_id)
        .cloned()
        .unwrap_or_else(|| short_id(&route.owner_id));

    // Recent journal entries for this route. We pull a page off the
    // group's journal and keep only those that targeted this route_id.
    // 10 is enough for the "recent activity" panel; the full view
    // lands with the journal page later.
    let mut journal_view: Vec<RouteDetailJournalRow> = Vec::new();
    if let Ok(entries) = state.journal().list_for_group(
        &route.group_id,
        crate::journal::ListCursor {
            before: None,
            limit: 50,
        },
    ) {
        for entry in entries
            .into_iter()
            .filter(|e| e.route_id == route.id)
            .take(10)
        {
            journal_view.push(RouteDetailJournalRow {
                number: entry.number,
                status: entry.response.status,
                status_class: status_class(entry.response.status),
                duration_ms: entry.duration_ms,
                created_at: entry.created_at.to_rfc3339(),
                trace_id: entry.trace_id.clone(),
                trace_id_short: entry
                    .trace_id
                    .as_ref()
                    .map(|t| t.chars().take(8).collect::<String>())
                    .unwrap_or_default(),
            });
        }
    }

    let route_view = RouteDetailRoute {
        id: route.id.clone(),
        group_name: route.group_name.clone(),
        number: route.number,
        methods: route.methods.join(", "),
        first_method: route
            .methods
            .first()
            .cloned()
            .unwrap_or_else(|| "GET".into()),
        path: route.path.clone(),
        url: format!(
            "{}{}",
            crate::auth_api::group_base_url(
                &route.group_name,
                &headers,
                state.trust_forwarded_headers()
            ),
            route.path
        ),
        language: route.language.clone(),
        bindings_version: route.bindings_version.clone(),
        component_size_human: human_size(route.compiled_wasm.len()),
        owner_name,
        hits_total: route.hits_total,
        last_hit_at: route.last_hit_at.map(|t| t.to_rfc3339()),
        created_at: route.created_at.to_rfc3339(),
        source: route.source.clone(),
    };

    render(
        &state,
        "route_detail.html",
        context! {
            page_title => format!("{} {}", route.methods.join(", "), route.path),
            user => UserBadge::from(&auth),
            route => route_view,
            journal => journal_view,
        },
    )
}

#[derive(Serialize)]
struct RouteDetailRoute {
    id: String,
    group_name: String,
    number: u32,
    methods: String,
    first_method: String,
    path: String,
    /// Full public URL the SUT calls: `{scheme}://{group}.{apex}{path}`.
    url: String,
    language: String,
    bindings_version: String,
    component_size_human: String,
    owner_name: String,
    hits_total: u64,
    last_hit_at: Option<String>,
    created_at: String,
    source: Option<String>,
}

#[derive(Serialize)]
struct RouteDetailJournalRow {
    number: u32,
    status: u16,
    status_class: &'static str,
    duration_ms: u64,
    created_at: String,
    trace_id: Option<String>,
    trace_id_short: String,
}

fn status_class(status: u16) -> &'static str {
    match status / 100 {
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

fn human_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        return format!("{bytes} B");
    }
    if b < KIB * KIB {
        return format!("{:.1} KiB", b / KIB);
    }
    format!("{:.1} MiB", b / (KIB * KIB))
}

fn ui_not_found(state: &AppState, auth: &AuthContext, what: &str) -> Response {
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Not found",
            api_hint => format!("{what} doesn't exist (or you can't see it)."),
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::NOT_FOUND;
    resp
}

// -- /__ui/journal/live -----------------------------------------------------
//
// Streaming view backed by `GET /__api/journal/tail` (slice 11 SSE).
// The page pre-renders the most recent N entries server-side so the
// table is populated on first paint; a small inline EventSource script
// then prepends rows as the SSE delivers new `handled` events. No
// HTMX dependency yet — plain JS is enough for "listen to a stream
// and append to a list."
//
// Auth: same rule as the SSE endpoint underneath. With ?group=, the
// caller must be admin or own a route in that group (403 otherwise).
// Without ?group=, admin-only (host-wide tail). Non-admin without
// ?group= sees the picker but no tail.

#[derive(Debug, Deserialize, Default)]
struct UiLiveJournalQuery {
    group: Option<String>,
    method: Option<String>,
    path_pattern: Option<String>,
    status: Option<String>,
}

async fn live_journal_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(raw_q): Query<UiLiveJournalQuery>,
) -> Response {
    // Form-submitted "Any method" sends `method=` (an empty value),
    // which deserialises as `Some("")` and would filter out every
    // entry under the naive `eq_ignore_ascii_case("")` check below.
    // Normalise once up front so every downstream consumer (filter,
    // SSE-URL builder, `any_filter_active`, form echo) sees `None`
    // when the user picked the empty option.
    let q = UiLiveJournalQuery {
        group: nonempty(raw_q.group.as_deref()),
        method: nonempty(raw_q.method.as_deref()),
        path_pattern: nonempty(raw_q.path_pattern.as_deref()),
        status: nonempty(raw_q.status.as_deref()),
    };

    // Resolve the group of available groups for the picker. Admins
    // see all groups by name; non-admins see only their owned groups.
    let registry = state.routes().registry();
    let available_groups: Vec<String> = if auth.is_admin {
        registry
            .list_groups()
            .unwrap_or_default()
            .into_iter()
            .map(|g| g.name)
            .collect()
    } else {
        registry
            .list_groups_by_owner(&auth.user_id)
            .unwrap_or_default()
            .into_iter()
            .map(|g| g.name)
            .collect()
    };

    let group_param = nonempty(q.group.as_deref());

    // Authorization for tailing this scope. Mirrors `tail_journal`:
    //   * group set → caller must be admin or own a route in the group
    //   * group unset → admin-only (host-wide tail)
    let (scope, can_tail, resolved_group_id): (&str, bool, Option<String>) = match &group_param {
        Some(name) => match registry.read_group_by_ref(name) {
            Ok(g) => match caller_can_view_group(&state, &auth, &g.id) {
                true => ("group", true, Some(g.id)),
                false => return forbidden_page(&state, &auth),
            },
            Err(_) => return ui_not_found(&state, &auth, &format!("Group {name}")),
        },
        None if auth.is_admin => ("host", true, None),
        None => ("none", false, None),
    };

    // Pre-fetch a window of recent entries so the table isn't empty
    // on first paint (or after navigating away and back). Window size
    // is generous (200 raw) so even narrow filters usually have
    // something to show. Group-scoped: read the group's journal
    // directly. Host-wide (admin only): fan out across all groups,
    // union, sort by created_at desc, take the head. Filter
    // method/path_pattern/status in-process to match what the SSE
    // tail will deliver next.
    const RAW_WINDOW: usize = 200;
    const DISPLAY_LIMIT: usize = 50;
    let initial = match (resolved_group_id.as_deref(), group_param.as_deref()) {
        (Some(gid), Some(name)) => fetch_recent_for_group(&state, gid, name, RAW_WINDOW),
        (None, None) if auth.is_admin => fetch_recent_host_wide(&state, RAW_WINDOW),
        _ => Vec::new(),
    };
    let initial: Vec<LiveJournalRow> = initial
        .into_iter()
        .filter(|(r, _)| match q.method.as_deref() {
            Some(m) => r.request.method.eq_ignore_ascii_case(m),
            None => true,
        })
        .filter(|(r, _)| match q.status.as_deref() {
            Some(s) => status_matches(r.response.status, s),
            None => true,
        })
        .filter(|(r, _)| match q.path_pattern.as_deref() {
            Some(p) => glob_match_simple(p, &r.request.path),
            None => true,
        })
        .take(DISPLAY_LIMIT)
        .map(|(r, name)| LiveJournalRow::from_record(&r, &name))
        .collect();

    let sse_url = build_sse_url(group_param.as_deref(), &q);
    let any_filter_active = q.method.is_some() || q.path_pattern.is_some() || q.status.is_some();

    render(
        &state,
        "live_journal.html",
        context! {
            page_title => "Live journal",
            user => UserBadge::from(&auth),
            scope => scope,
            can_tail => can_tail,
            group => group_param.unwrap_or_default(),
            method => q.method.clone().unwrap_or_default(),
            path_pattern => q.path_pattern.clone().unwrap_or_default(),
            status => q.status.clone().unwrap_or_default(),
            any_filter_active => any_filter_active,
            available_groups => available_groups,
            methods => ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
            initial_entries => initial,
            sse_url => sse_url,
        },
    )
}

/// Fetch up to `limit` most-recent journal records for one group,
/// pairing each with the group's display name so the row template
/// has what it needs without a per-row lookup.
fn fetch_recent_for_group(
    state: &AppState,
    group_id: &str,
    group_name: &str,
    limit: usize,
) -> Vec<(crate::journal::JournalRecord, String)> {
    state
        .journal()
        .list_for_group(
            group_id,
            crate::journal::ListCursor {
                before: None,
                limit,
            },
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r, group_name.to_string()))
        .collect()
}

/// Host-wide fan-out for the admin tail's pre-fetch: pull a window
/// of recent entries from every group, union them, sort by
/// `created_at` desc, return the head. Bounded by the per-group
/// window (`per_group`) so a host with hundreds of groups doesn't
/// pay an unbounded cost — at worst N_groups × per_group reads,
/// then a sort. The SSE tail keeps the view current after this
/// initial paint.
fn fetch_recent_host_wide(
    state: &AppState,
    limit: usize,
) -> Vec<(crate::journal::JournalRecord, String)> {
    let registry = state.routes().registry();
    let groups = match registry.list_groups() {
        Ok(gs) => gs,
        Err(_) => return Vec::new(),
    };
    // 20 entries per group is enough to cover the common case (a
    // few hot groups dominating the host-wide tail) without
    // ballooning the read count on a quiet host with many groups.
    let per_group = 20.min(limit);
    let mut all: Vec<(crate::journal::JournalRecord, String)> = Vec::new();
    for g in groups {
        all.extend(fetch_recent_for_group(state, &g.id, &g.name, per_group));
    }
    all.sort_by_key(|p| std::cmp::Reverse(p.0.created_at));
    all.truncate(limit);
    all
}

fn caller_can_view_group(state: &AppState, auth: &AuthContext, group_id: &str) -> bool {
    if auth.is_admin {
        return true;
    }
    state
        .routes()
        .registry()
        .list_routes_by_owner(&auth.user_id)
        .map(|routes| routes.iter().any(|r| r.group_id == group_id))
        .unwrap_or(false)
}

fn status_matches(actual: u16, pattern: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    if let Ok(n) = p.parse::<u16>() {
        return actual == n;
    }
    if p.len() == 3 && p.ends_with("xx") {
        let bucket = match p.as_bytes()[0] {
            b'1'..=b'5' => (p.as_bytes()[0] - b'0') as u16,
            _ => return false,
        };
        return actual / 100 == bucket;
    }
    false
}

/// Tiny `*` glob used by the live-journal in-process filter. Matches
/// the same shape `JournalFilter::matches` uses on the SSE side; we
/// reimplement locally rather than wire the host's internal filter
/// into the UI surface.
fn glob_match_simple(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    if !value.starts_with(first) {
        return false;
    }
    let mut pos = first.len();
    let mut peek = parts.next();
    while let Some(part) = peek {
        peek = parts.next();
        if peek.is_none() {
            return value[pos..].ends_with(part);
        }
        match value[pos..].find(part) {
            Some(idx) => pos += idx + part.len(),
            None => return false,
        }
    }
    true
}

fn build_sse_url(group: Option<&str>, q: &UiLiveJournalQuery) -> String {
    let mut parts: Vec<(String, String)> = Vec::new();
    if let Some(g) = group {
        parts.push(("group".into(), g.to_string()));
    }
    if let Some(m) = q.method.as_deref().and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    }) {
        parts.push(("method".into(), m.to_string()));
    }
    if let Some(p) = q.path_pattern.as_deref().and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    }) {
        parts.push(("path_pattern".into(), p.to_string()));
    }
    if let Some(s) = q.status.as_deref().and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    }) {
        parts.push(("status".into(), s.to_string()));
    }
    if parts.is_empty() {
        "/__api/journal/tail".into()
    } else {
        format!("/__api/journal/tail?{}", encode_query(&parts))
    }
}

#[derive(Serialize)]
struct LiveJournalRow {
    number: u32,
    group_name: String,
    method: String,
    path: String,
    status: u16,
    status_class: &'static str,
    duration_ms: u64,
    created_at: String,
    created_at_human: String,
    trace_id_short: String,
}

impl LiveJournalRow {
    fn from_record(r: &crate::journal::JournalRecord, group_name: &str) -> Self {
        Self {
            number: r.number,
            group_name: group_name.to_string(),
            method: r.request.method.clone(),
            path: r.request.path.clone(),
            status: r.response.status,
            status_class: status_class(r.response.status),
            created_at: r.created_at.to_rfc3339(),
            created_at_human: r.created_at.format("%H:%M:%S").to_string(),
            duration_ms: r.duration_ms,
            trace_id_short: r
                .trace_id
                .as_deref()
                .map(|t| t.chars().take(8).collect::<String>())
                .unwrap_or_default(),
        }
    }
}

// -- /__ui/journal/{group}/{n} ----------------------------------------------
//
// Full record for one journal entry: request envelope, response
// envelope, handler logs, timing. Read-only; deletion happens via
// TTL on the journal record.

async fn journal_entry_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
) -> Response {
    let group = match state.routes().registry().read_group_by_ref(&group_ref) {
        Ok(g) => g,
        Err(_) => return ui_not_found(&state, &auth, &format!("Group {group_ref}")),
    };
    if !caller_can_view_group(&state, &auth, &group.id) {
        return forbidden_page(&state, &auth);
    }
    let entry = match state.journal().get(&group.id, number) {
        Ok(e) => e,
        Err(_) => {
            return ui_not_found(
                &state,
                &auth,
                &format!("Journal entry {group_ref}/{number}"),
            );
        }
    };

    let view = JournalEntryView::from_record(&entry);
    render(
        &state,
        "journal_entry.html",
        context! {
            page_title => format!("Journal {}/{}", group_ref, number),
            user => UserBadge::from(&auth),
            entry => view,
        },
    )
}

#[derive(Serialize)]
struct JournalEntryView {
    id: String,
    number: u32,
    group_name: String,
    route_number: u32,
    method: String,
    path: String,
    status: u16,
    status_class: &'static str,
    duration_ms: u64,
    created_at: String,
    trace_id: Option<String>,
    matched_pattern: String,
    path_params: Vec<(String, String)>,
    query: Vec<(String, String)>,
    error: Option<String>,
    dropped_response_headers: Vec<String>,
    request_headers: Vec<(String, String)>,
    request_body_text: Option<String>,
    request_body_truncated: bool,
    request_body_original_size: usize,
    response_headers: Vec<(String, String)>,
    response_body_text: Option<String>,
    response_body_truncated: bool,
    response_body_original_size: usize,
    handler_logs: Vec<HandlerLogView>,
}

#[derive(Serialize)]
struct HandlerLogView {
    level: String,
    message: String,
    timestamp: String,
}

impl JournalEntryView {
    fn from_record(r: &crate::journal::JournalRecord) -> Self {
        Self {
            id: r.id.clone(),
            number: r.number,
            group_name: r.group_name.clone(),
            route_number: r.route_number,
            method: r.request.method.clone(),
            path: r.request.path.clone(),
            status: r.response.status,
            status_class: status_class(r.response.status),
            duration_ms: r.duration_ms,
            created_at: r.created_at.to_rfc3339(),
            trace_id: r.trace_id.clone(),
            matched_pattern: r.matched_pattern.clone(),
            path_params: r.path_params.clone(),
            query: r.query.clone(),
            error: r.error.clone(),
            dropped_response_headers: r.dropped_response_headers.clone(),
            request_headers: r.request.headers.clone(),
            request_body_text: body_as_text(&r.request.body),
            request_body_truncated: r.request.body_truncated,
            request_body_original_size: r.request.original_body_size,
            response_headers: r.response.headers.clone(),
            response_body_text: body_as_text(&r.response.body),
            response_body_truncated: r.response.body_truncated,
            response_body_original_size: r.response.original_body_size,
            handler_logs: r
                .handler_logs
                .iter()
                .map(|l| HandlerLogView {
                    level: l.level.clone(),
                    message: l.message.clone(),
                    timestamp: l.timestamp.to_rfc3339(),
                })
                .collect(),
        }
    }
}

/// Render a body as UTF-8 if it decodes cleanly; `None` if it's empty
/// or looks binary. The journal pre-truncates large bodies for us, so
/// this never needs to bound the output length itself.
fn body_as_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(s)
            if !s
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t') =>
        {
            Some(s.to_string())
        }
        _ => Some(format!("(binary, {} bytes)", bytes.len())),
    }
}

// -- State pages (slice 27) -------------------------------------------------
//
// Read-only inspection of the per-route and per-group kv namespaces
// + a clear-state action. Reuses the registry's existing list helpers
// (`list_route_state`, plus the new `list_group_state` added for the
// gkv: namespace). Owner-or-admin gated. Clear-state goes through
// CSRF since it mutates.

async fn group_state_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
) -> Response {
    let group = match resolve_owned_group(&state, &auth, &group_ref) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    let entries = match state.routes().registry().list_group_state(&group.id) {
        Ok(e) => e,
        Err(e) => return ui_error_500(&state, &auth, format!("list group state: {e}")),
    };
    let view = StatePageData::from_entries(&entries);
    render(
        &state,
        "group_state.html",
        context! {
            page_title => format!("Group state: {}", group.name),
            user => UserBadge::from(&auth),
            group_name => group.name,
            data => view,
        },
    )
}

async fn group_state_clear_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group_ref): Path<String>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    let group = match resolve_owned_group(&state, &auth, &group_ref) {
        Ok(g) => g,
        Err(resp) => return *resp,
    };
    if let Err(e) = state.routes().registry().clear_group_state(&group.id) {
        return ui_error_500(&state, &auth, format!("clear: {e}"));
    }
    axum::response::Redirect::to(&format!("/__ui/groups/{}/state", group.name)).into_response()
}

async fn route_state_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
) -> Response {
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => {
            return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}"));
        }
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }
    let entries = match state
        .routes()
        .registry()
        .list_route_state(&group_ref, number)
    {
        Ok(e) => e,
        Err(e) => return ui_error_500(&state, &auth, format!("list route state: {e}")),
    };
    let view = StatePageData::from_entries(&entries);
    render(
        &state,
        "route_state.html",
        context! {
            page_title => format!("Route state: {}/{}", route.group_name, number),
            user => UserBadge::from(&auth),
            group_name => route.group_name,
            route_number => route.number,
            route_methods => route.methods.join(", "),
            route_path => route.path,
            data => view,
        },
    )
}

async fn route_state_clear_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => {
            return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}"));
        }
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }
    if let Err(e) = state
        .routes()
        .registry()
        .clear_route_state(&group_ref, number)
    {
        return ui_error_500(&state, &auth, format!("clear: {e}"));
    }
    axum::response::Redirect::to(&format!(
        "/__ui/routes/{}/{}/state",
        route.group_name, number
    ))
    .into_response()
}

// -- Dry-run page -----------------------------------------------------------
//
// UI wrapper around `POST /__api/routes/{group}/{n}/dry-run`. GET renders
// an empty form (method/path/headers/body); POST calls `dry_run::dry_run`
// directly and re-renders the same page with the response card filled in.
// Owner-or-admin gated, same rule as the REST surface. CSRF on the POST.

#[derive(serde::Deserialize)]
struct DryRunForm {
    #[serde(rename = "_csrf")]
    _csrf: String,
    method: String,
    path: String,
    #[serde(default)]
    headers: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    kv_overrides: String,
    #[serde(default)]
    gkv_overrides: String,
}

#[derive(Serialize, Default, Clone)]
struct DryRunFormState {
    method: String,
    path: String,
    headers: String,
    query: String,
    body: String,
    kv_overrides: String,
    gkv_overrides: String,
}

#[derive(Serialize)]
struct DryRunResponseView {
    status: u16,
    status_class: &'static str,
    duration_ms: u64,
    snapshot_keys: u64,
    headers: Vec<(String, String)>,
    body_text: Option<String>,
    body_original_size: usize,
    handler_logs: Vec<HandlerLogView>,
    error: Option<String>,
}

async fn route_dry_run_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
) -> Response {
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}")),
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }
    let form = DryRunFormState {
        method: route
            .methods
            .first()
            .cloned()
            .unwrap_or_else(|| "POST".into()),
        path: route.path.clone(),
        ..DryRunFormState::default()
    };
    render_dry_run(&state, &auth, &route, form, None, None)
}

async fn route_dry_run_submit(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
    axum::Form(form): axum::Form<DryRunForm>,
) -> Response {
    let _ = form._csrf;

    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}")),
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }

    let form_state = DryRunFormState {
        method: form.method.clone(),
        path: form.path.clone(),
        headers: form.headers.clone(),
        query: form.query.clone(),
        body: form.body.clone(),
        kv_overrides: form.kv_overrides.clone(),
        gkv_overrides: form.gkv_overrides.clone(),
    };

    let headers = match parse_kv_lines(&form.headers, ':') {
        Ok(v) => v,
        Err(msg) => {
            return render_dry_run(
                &state,
                &auth,
                &route,
                form_state,
                Some(format!("Headers: {msg}")),
                None,
            );
        }
    };
    let query = match parse_kv_lines(&form.query, '=') {
        Ok(v) => v,
        Err(msg) => {
            return render_dry_run(
                &state,
                &auth,
                &route,
                form_state,
                Some(format!("Query: {msg}")),
                None,
            );
        }
    };
    if !form.path.starts_with('/') {
        return render_dry_run(
            &state,
            &auth,
            &route,
            form_state,
            Some("Path must start with /".into()),
            None,
        );
    }

    let kv_overrides = match parse_kv_lines(&form.kv_overrides, '=') {
        Ok(pairs) => pairs
            .into_iter()
            .map(|(k, v)| (k, WireBytes::Text(v)))
            .collect(),
        Err(msg) => {
            return render_dry_run(
                &state,
                &auth,
                &route,
                form_state,
                Some(format!("kv overrides: {msg}")),
                None,
            );
        }
    };
    let gkv_overrides = match parse_kv_lines(&form.gkv_overrides, '=') {
        Ok(pairs) => pairs
            .into_iter()
            .map(|(k, v)| (k, WireBytes::Text(v)))
            .collect(),
        Err(msg) => {
            return render_dry_run(
                &state,
                &auth,
                &route,
                form_state,
                Some(format!("gkv overrides: {msg}")),
                None,
            );
        }
    };

    let request = crate::dry_run::DryRunRequest {
        method: form.method.clone(),
        path: form.path.clone(),
        headers,
        body: form.body.as_bytes().to_vec(),
        path_params: None,
        query,
        kv_overrides,
        gkv_overrides,
    };
    let response = match crate::dry_run::dry_run(
        state.runtime().clone(),
        state.routes().clone(),
        route.clone(),
        request,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return render_dry_run(
                &state,
                &auth,
                &route,
                form_state,
                Some(format!("Dry-run failed: {e}")),
                None,
            );
        }
    };

    let view = DryRunResponseView {
        status: response.status,
        status_class: status_class(response.status),
        duration_ms: response.duration_ms,
        snapshot_keys: response.snapshot_keys,
        headers: response.headers,
        body_text: body_as_text(&response.body),
        body_original_size: response.body.len(),
        handler_logs: response
            .handler_logs
            .into_iter()
            .map(|l| HandlerLogView {
                level: l.level,
                message: l.message,
                timestamp: l.timestamp.to_rfc3339(),
            })
            .collect(),
        error: response.error,
    };
    render_dry_run(&state, &auth, &route, form_state, None, Some(view))
}

fn render_dry_run(
    state: &AppState,
    auth: &AuthContext,
    route: &Route,
    form: DryRunFormState,
    error: Option<String>,
    response: Option<DryRunResponseView>,
) -> Response {
    let had_error = error.is_some();
    let mut resp = render(
        state,
        "route_dry_run.html",
        context! {
            page_title => format!("Dry-run: {} {}", route.methods.join(", "), route.path),
            user => UserBadge::from(auth),
            route_group_name => &route.group_name,
            route_number => route.number,
            route_methods => route.methods.join(", "),
            route_path => &route.path,
            methods => ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
            form => form,
            error => error,
            response => response,
        },
    );
    if had_error {
        *resp.status_mut() = StatusCode::BAD_REQUEST;
    }
    resp
}

// -- Source editor (slice 40) ------------------------------------------------
//
// GET /__ui/routes/{group}/{n}/source/edit renders a textarea pre-
// populated with the route's stored source. POST submits the new
// source through `api::patch_route_core`, which recompiles via the
// sidecar and swaps the artifact atomically. On success we redirect
// back to the detail page; on failure (most commonly compile_failed
// with diagnostics) we re-render the form with the error inline and
// the user's edits intact.
//
// Only source-language routes can be edited here. wasm-uploaded
// routes have `source: None` — the form has no content to start
// from and the host can't recompile wasm-from-wasm via the sidecar,
// so we 404 those rather than offer a misleading affordance.

#[derive(Deserialize)]
struct SourceEditForm {
    _csrf: String,
    source: String,
    /// New language for the source. Defaults to the route's existing
    /// language when the form is rendered, but the user can flip TS ↔
    /// JS without re-creating the route. Switching to `wasm` isn't
    /// allowed through this surface — pre-compiled wasm uploads come
    /// via REST.
    language: Option<String>,
}

#[derive(Serialize)]
struct SourceEditError {
    title: String,
    message: String,
    diagnostics: Vec<String>,
}

async fn route_source_edit_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
) -> Response {
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}")),
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }
    let Some(current) = route.source.clone() else {
        return ui_not_found(
            &state,
            &auth,
            "Route was uploaded as pre-compiled wasm; nothing to edit here.",
        );
    };
    render_source_edit(&state, &auth, &route, current, None)
}

async fn route_source_edit_submit(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
    axum::Form(form): axum::Form<SourceEditForm>,
) -> Response {
    let _ = form._csrf;
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}")),
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }
    if route.source.is_none() {
        return ui_not_found(
            &state,
            &auth,
            "Route was uploaded as pre-compiled wasm; nothing to edit here.",
        );
    }

    // Guard against unexpected form values — the dropdown only offers
    // typescript/javascript, but a hand-crafted POST could send anything
    // and we don't want to silently swap to "wasm" (which would need a
    // separate compiled_wasm upload, not a source recompile).
    let language = form
        .language
        .clone()
        .filter(|l| matches!(l.as_str(), "typescript" | "javascript"))
        .unwrap_or_else(|| route.language.clone());
    let body = crate::api::PatchRouteBody {
        methods: None,
        path: None,
        language: Some(language),
        source: Some(form.source.clone()),
    };
    match crate::api::patch_route_core(&state, &auth, &group_ref, number, body).await {
        Ok(_updated) => {
            let location = format!("/__ui/routes/{group_ref}/{number}");
            let mut resp = Response::default();
            *resp.status_mut() = StatusCode::SEE_OTHER;
            resp.headers_mut().insert(
                axum::http::header::LOCATION,
                axum::http::HeaderValue::try_from(location).expect("ascii location"),
            );
            resp
        }
        Err(api_err) => {
            let title = match api_err.code() {
                "compile_failed" => "Compile failed".to_string(),
                _ => "Couldn't update source".to_string(),
            };
            render_source_edit(
                &state,
                &auth,
                &route,
                form.source,
                Some(SourceEditError {
                    title,
                    message: api_err.message().to_string(),
                    diagnostics: api_err.diagnostics().to_vec(),
                }),
            )
        }
    }
}

fn render_source_edit(
    state: &AppState,
    auth: &AuthContext,
    route: &Route,
    source: String,
    error: Option<SourceEditError>,
) -> Response {
    let had_error = error.is_some();
    let mut resp = render(
        state,
        "route_source_edit.html",
        context! {
            page_title => format!("Edit source: {} {}", route.methods.join(", "), route.path),
            user => UserBadge::from(auth),
            route_group_name => &route.group_name,
            route_number => route.number,
            route_methods => route.methods.join(", "),
            route_path => &route.path,
            route_language => &route.language,
            source => source,
            error => error,
        },
    );
    if had_error {
        *resp.status_mut() = StatusCode::BAD_REQUEST;
    }
    resp
}

/// Parse a multi-line textarea into key/value pairs, splitting each
/// non-empty line on the first `sep`. Leading/trailing whitespace
/// around both key and value is trimmed. Empty lines and lines that
/// are just whitespace are skipped. Returns the original line's text
/// in the error message when parsing fails so users can find the
/// offender.
fn parse_kv_lines(input: &str, sep: char) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for raw in input.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(sep) else {
            return Err(format!("missing `{sep}` in line: {line:?}"));
        };
        let k = k.trim();
        if k.is_empty() {
            return Err(format!("empty key in line: {line:?}"));
        }
        out.push((k.to_string(), v.trim().to_string()));
    }
    Ok(out)
}

#[derive(Serialize)]
struct StatePageData {
    entries: Vec<StateEntryView>,
    total: usize,
}

impl StatePageData {
    fn from_entries(entries: &[crate::registry::RouteStateEntry]) -> Self {
        Self {
            entries: entries.iter().map(StateEntryView::from).collect(),
            total: entries.len(),
        }
    }
}

#[derive(Serialize)]
struct StateEntryView {
    key: String,
    kind: String,
    /// Decoded bytes value as text when it parses cleanly as UTF-8;
    /// the raw byte count otherwise. Inline display only — large
    /// values are still bounded by the kv store's own limits.
    value_text: Option<String>,
    value_size: Option<usize>,
    length: Option<u64>,
}

impl From<&crate::registry::RouteStateEntry> for StateEntryView {
    fn from(e: &crate::registry::RouteStateEntry) -> Self {
        let (value_text, value_size) = match &e.value {
            Some(bytes) => {
                let size = bytes.len();
                let text = std::str::from_utf8(bytes)
                    .ok()
                    .filter(|s| {
                        !s.chars()
                            .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
                    })
                    .map(|s| s.to_string());
                (text, Some(size))
            }
            None => (None, None),
        };
        Self {
            key: e.key.clone(),
            kind: e.kind.clone(),
            value_text,
            value_size,
            length: e.length,
        }
    }
}

// -- Route action handlers (slice 26) ---------------------------------------

async fn route_delete_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group_ref, number)): Path<(String, u32)>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    let route = match state
        .routes()
        .registry()
        .get_route_by_slug(&group_ref, number)
    {
        Ok(r) => r,
        Err(_) => {
            return ui_not_found(&state, &auth, &format!("Route {group_ref}/{number}"));
        }
    };
    if !auth.is_admin && route.owner_id != auth.user_id {
        return forbidden_page(&state, &auth);
    }
    if let Err(e) = state.routes().registry().delete_route(&group_ref, number) {
        return ui_error_500(&state, &auth, format!("delete: {e}"));
    }
    state.routes().refresh_after_delete(&route.id);
    // Redirect back to the group's detail page if the group still
    // exists (implicit single-route groups vanish along with their
    // sole route), otherwise the listing.
    let landing = if state
        .routes()
        .registry()
        .read_group_by_ref(&group_ref)
        .is_ok()
    {
        format!("/__ui/groups/{group_ref}")
    } else {
        "/__ui/groups".to_string()
    };
    axum::response::Redirect::to(&landing).into_response()
}

// -- /__ui/me/tokens --------------------------------------------------------
//
// Self-service API token management. Lists the caller's own tokens,
// lets them create a new one (plaintext shown once on the response),
// and revoke by name. Admins managing other users' tokens still go
// through the CLI; this surface is deliberately "your own" only.
//
// First authed UI forms in the codebase — CSRF middleware checks every
// POST here. The form template embeds `{{ csrf_token }}` and the
// middleware compares it to the wm_csrf cookie set on the matching GET.

#[derive(serde::Deserialize)]
struct CreateTokenForm {
    name: String,
    /// One of `never` / `30d` / `90d` / `1y` / `custom`. When `custom`
    /// (or absent), `ttl_hours` is consulted. The empty string is
    /// treated as `custom` so the old form (no preset, just hours)
    /// keeps working.
    ttl_preset: Option<String>,
    ttl_hours: Option<String>,
    /// Validated by the CSRF middleware; ignored here, only present so
    /// `axum::Form` doesn't reject the request as malformed.
    #[serde(rename = "_csrf")]
    _csrf: String,
}

#[derive(serde::Deserialize, Default)]
struct UiTokensQuery {
    sort: Option<String>,
    dir: Option<String>,
}

#[derive(serde::Deserialize)]
struct CsrfOnlyForm {
    #[serde(rename = "_csrf")]
    _csrf: String,
}

async fn tokens_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<UiTokensQuery>,
) -> Response {
    let mut tokens = match state.auth().list_tokens_for(&auth.user_id) {
        Ok(ts) => ts,
        Err(e) => return ui_error_500(&state, &auth, format!("list tokens: {e}")),
    };
    let (sort, dir) = resolve_token_sort(q.sort.as_deref(), q.dir.as_deref());
    sort_tokens(&mut tokens, sort, dir);
    let grants = load_oauth_grants(&state, &auth);
    let data = TokensPageData::list_with_sort(&tokens, grants, sort, dir);
    render_tokens_page(&state, &auth, data)
}

fn resolve_token_sort<'a>(sort: Option<&'a str>, dir: Option<&'a str>) -> (&'a str, &'a str) {
    let sort = match sort {
        Some(s) if matches!(s, "name" | "created" | "expires" | "last_used") => s,
        _ => "created",
    };
    let dir = match dir {
        Some("asc") => "asc",
        _ => "desc",
    };
    (sort, dir)
}

fn sort_tokens(tokens: &mut [crate::auth::Token], sort: &str, dir: &str) {
    use std::cmp::Ordering;
    let cmp: fn(&crate::auth::Token, &crate::auth::Token) -> Ordering = match sort {
        "name" => |a, b| a.name.cmp(&b.name),
        "expires" => |a, b| {
            // None means "never expires" — sort as the largest value
            // so it sits at the end ascending, the start descending.
            match (a.expires_at, b.expires_at) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(x), Some(y)) => x.cmp(&y),
            }
        },
        "last_used" => |a, b| a.last_used_at.cmp(&b.last_used_at),
        _ => |a, b| a.created_at.cmp(&b.created_at),
    };
    tokens.sort_by(|a, b| if dir == "asc" { cmp(a, b) } else { cmp(b, a) });
}

async fn create_token_form(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::Form(form): axum::Form<CreateTokenForm>,
) -> Response {
    let name = form.name.trim();
    if name.is_empty() {
        return tokens_page_with_error(&state, &auth, "Name is required.");
    }
    const DAY: u64 = 86_400;
    let ttl = match form.ttl_preset.as_deref().map(str::trim) {
        Some("never") => None,
        Some("30d") => Some(30 * DAY),
        Some("90d") => Some(90 * DAY),
        Some("1y") => Some(365 * DAY),
        // "custom", "", or absent: fall through to the hours field.
        // The old form (no preset, just hours) keeps working.
        _ => match form.ttl_hours.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(s) => match s.parse::<u64>() {
                Ok(h) if h > 0 => Some(h * 3600),
                _ => {
                    return tokens_page_with_error(
                        &state,
                        &auth,
                        "TTL hours must be a positive integer.",
                    );
                }
            },
        },
    };

    let (token, plaintext) = match state.auth().create_token(&auth.user_id, name, ttl) {
        Ok(pair) => pair,
        Err(crate::auth::AuthError::NameTaken(n)) => {
            return tokens_page_with_error(
                &state,
                &auth,
                &format!("A token named {n:?} already exists. Pick a different name."),
            );
        }
        Err(e) => return ui_error_500(&state, &auth, format!("create token: {e}")),
    };
    let _ = token;

    let tokens = state
        .auth()
        .list_tokens_for(&auth.user_id)
        .map(|mut ts| {
            ts.sort_by_key(|t| std::cmp::Reverse(t.created_at));
            ts
        })
        .unwrap_or_default();
    let grants = load_oauth_grants(&state, &auth);
    render_tokens_page(
        &state,
        &auth,
        TokensPageData::after_create(&tokens, grants, &plaintext, name),
    )
}

async fn revoke_token_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(name): Path<String>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    // CSRF middleware has already validated `_csrf` for us; the form
    // struct just exists so axum's Form extractor doesn't reject the
    // request body.
    match state.auth().revoke_token_by_name(&auth.user_id, &name) {
        Ok(_) => {}
        Err(e) => return ui_error_500(&state, &auth, format!("revoke: {e}")),
    }
    // 303 See Other so a browser refresh after revoke doesn't replay
    // the POST.
    axum::response::Redirect::to("/__ui/me/tokens").into_response()
}

/// Revoke every active OAuth grant for `(caller, client_id)`. Marks
/// the matching refresh tokens revoked so they can't be rotated;
/// existing access tokens keep working until their TTL expires
/// (1 hour worst-case) — there's no per-user index on access tokens
/// today, so we rely on TTL rather than enumerate.
async fn revoke_oauth_grant_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(client_id): Path<String>,
    axum::Form(_form): axum::Form<CsrfOnlyForm>,
) -> Response {
    let mut bucket = match state.auth().storage().admin_bucket() {
        Ok(b) => b,
        Err(e) => return ui_error_500(&state, &auth, format!("open bucket: {e}")),
    };
    match crate::mcp_oauth::revoke_grants_for_client(&mut bucket, &auth.user_id, &client_id) {
        Ok(_n) => {}
        Err(e) => return ui_error_500(&state, &auth, format!("revoke oauth grant: {e}")),
    }
    axum::response::Redirect::to("/__ui/me/tokens").into_response()
}

#[derive(serde::Deserialize)]
struct RenameTokenForm {
    new_name: String,
    #[serde(rename = "_csrf")]
    _csrf: String,
}

async fn rename_token_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(old_name): Path<String>,
    axum::Form(form): axum::Form<RenameTokenForm>,
) -> Response {
    let new_name = form.new_name.trim();
    if new_name.is_empty() {
        return tokens_page_with_error(&state, &auth, "New name must not be empty.");
    }
    match state
        .auth()
        .rename_token(&auth.user_id, &old_name, new_name)
    {
        Ok(_) => axum::response::Redirect::to("/__ui/me/tokens").into_response(),
        Err(crate::auth::AuthError::NotFound) => {
            tokens_page_with_error(&state, &auth, &format!("Token {old_name:?} not found."))
        }
        Err(crate::auth::AuthError::NameTaken(n)) => tokens_page_with_error(
            &state,
            &auth,
            &format!("A token named {n:?} already exists. Pick a different name."),
        ),
        Err(e) => ui_error_500(&state, &auth, format!("rename: {e}")),
    }
}

#[derive(Serialize)]
struct TokensPageData {
    tokens: Vec<TokenRow>,
    oauth_grants: Vec<OAuthGrantRow>,
    plaintext: Option<String>,
    plaintext_name: Option<String>,
    error: Option<String>,
    sort: String,
    dir: String,
    dir_arrow: &'static str,
    sort_links: TokensSortLinks,
}

impl TokensPageData {
    fn list_with_sort(
        tokens: &[crate::auth::Token],
        grants: Vec<OAuthGrantRow>,
        sort: &str,
        dir: &str,
    ) -> Self {
        Self {
            tokens: tokens.iter().map(TokenRow::from).collect(),
            oauth_grants: grants,
            plaintext: None,
            plaintext_name: None,
            error: None,
            sort: sort.to_string(),
            dir: dir.to_string(),
            dir_arrow: arrow_for(dir),
            sort_links: TokensSortLinks::build(sort, dir),
        }
    }
    fn list(tokens: &[crate::auth::Token], grants: Vec<OAuthGrantRow>) -> Self {
        Self::list_with_sort(tokens, grants, "created", "desc")
    }
    fn after_create(
        tokens: &[crate::auth::Token],
        grants: Vec<OAuthGrantRow>,
        plaintext: &str,
        name: &str,
    ) -> Self {
        let mut data = Self::list(tokens, grants);
        data.plaintext = Some(plaintext.to_string());
        data.plaintext_name = Some(name.to_string());
        data
    }
    fn with_error(tokens: &[crate::auth::Token], grants: Vec<OAuthGrantRow>, msg: &str) -> Self {
        let mut data = Self::list(tokens, grants);
        data.error = Some(msg.to_string());
        data
    }
}

#[derive(Serialize)]
struct OAuthGrantRow {
    client_id: String,
    client_name: String,
    scope: String,
    granted_at: String,
    expires_at: String,
}

impl From<crate::mcp_oauth::OAuthGrantSummary> for OAuthGrantRow {
    fn from(g: crate::mcp_oauth::OAuthGrantSummary) -> Self {
        Self {
            client_id: g.client_id,
            client_name: g.client_name,
            scope: g.scope,
            granted_at: g.granted_at.to_rfc3339(),
            expires_at: g.expires_at.to_rfc3339(),
        }
    }
}

/// Best-effort load of the caller's active OAuth grants. Failures
/// surface as an empty list — the tokens page is more useful with
/// missing grants than a 500.
fn load_oauth_grants(state: &AppState, auth: &AuthContext) -> Vec<OAuthGrantRow> {
    let mut bucket = match state.auth().storage().admin_bucket() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "open admin bucket for oauth grants");
            return Vec::new();
        }
    };
    match crate::mcp_oauth::list_active_oauth_grants(&mut bucket, &auth.user_id) {
        Ok(grants) => grants.into_iter().map(OAuthGrantRow::from).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "list oauth grants");
            Vec::new()
        }
    }
}

#[derive(Serialize)]
struct TokensSortLinks {
    name: String,
    created: String,
    expires: String,
    last_used: String,
}

impl TokensSortLinks {
    fn build(current_sort: &str, current_dir: &str) -> Self {
        let mk = |col: &str, default_dir: &str| -> String {
            let dir = if current_sort == col {
                flip(current_dir)
            } else {
                default_dir
            };
            format!("/__ui/me/tokens?sort={col}&dir={dir}")
        };
        Self {
            name: mk("name", "asc"),
            created: mk("created", "desc"),
            expires: mk("expires", "asc"),
            last_used: mk("last_used", "desc"),
        }
    }
}

#[derive(Serialize)]
struct TokenRow {
    name: String,
    created_at: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
}

impl From<&crate::auth::Token> for TokenRow {
    fn from(t: &crate::auth::Token) -> Self {
        Self {
            name: t.name.clone(),
            created_at: t.created_at.to_rfc3339(),
            last_used_at: t.last_used_at.map(|x| x.to_rfc3339()),
            expires_at: t.expires_at.map(|x| x.to_rfc3339()),
        }
    }
}

fn render_tokens_page(state: &AppState, auth: &AuthContext, data: TokensPageData) -> Response {
    render(
        state,
        "tokens.html",
        context! {
            page_title => "API tokens",
            user => UserBadge::from(auth),
            data => data,
        },
    )
}

fn tokens_page_with_error(state: &AppState, auth: &AuthContext, error: &str) -> Response {
    let tokens = state
        .auth()
        .list_tokens_for(&auth.user_id)
        .unwrap_or_default();
    let grants = load_oauth_grants(state, auth);
    let data = TokensPageData::with_error(&tokens, grants, error);
    let mut resp = render_tokens_page(state, auth, data);
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

// -- Unmatched pages (slice 28) ---------------------------------------------
//
// The unmatched-request log surfaced at /__ui/unmatched, plus a
// per-entry detail page at /__ui/unmatched/{number}. Both admin-only,
// mirroring the REST surface at /__api/unmatched. Reuses the
// `JournalFilter::matches_unmatched` matcher for method + path_pattern
// filtering so the UI agrees with the REST shape.

#[derive(Deserialize)]
struct UiUnmatchedQuery {
    method: Option<String>,
    path_pattern: Option<String>,
    before: Option<u64>,
}

const UI_UNMATCHED_PAGE_LIMIT: usize = 25;

async fn unmatched_index_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(raw_q): Query<UiUnmatchedQuery>,
) -> Response {
    // ADR-0030 SemFLIP: any authed user may view; admin sees every
    // group's unmatched, a tenant sees only their own groups'.
    let visible: Option<std::collections::HashSet<String>> = if auth.is_admin {
        None
    } else {
        match state
            .routes()
            .registry()
            .list_groups_by_owner(&auth.user_id)
        {
            Ok(groups) => Some(groups.into_iter().map(|g| g.id).collect()),
            Err(e) => return ui_error_500(&state, &auth, format!("list groups: {e}")),
        }
    };
    let method = match nonempty(raw_q.method.as_deref()) {
        Some(m) => match validate_method(&m) {
            Ok(v) => Some(v),
            Err(e) => return ui_error_400(&state, &auth, format!("invalid method: {e}")),
        },
        None => None,
    };
    let path_pattern = nonempty(raw_q.path_pattern.as_deref());

    // Pull a generous raw window so filters still tend to have a page
    // worth of results even when they narrow aggressively. Fetch
    // limit+1 to know if there's an older page after this one.
    let any_filter = method.is_some() || path_pattern.is_some();
    let raw_limit = if any_filter {
        200
    } else {
        UI_UNMATCHED_PAGE_LIMIT + 1
    };
    let cursor = UnmatchedCursor {
        before: raw_q.before,
        limit: raw_limit,
    };
    let raw = match state.journal().list_unmatched(cursor, visible.as_ref()) {
        Ok(r) => r,
        Err(e) => return ui_error_500(&state, &auth, format!("list unmatched: {e}")),
    };

    let filter = JournalFilter {
        method: method.clone(),
        path_pattern: path_pattern.clone(),
        ..JournalFilter::default()
    };
    let filtered: Vec<_> = raw
        .into_iter()
        .filter(|r| filter.matches_unmatched(r))
        .collect();
    let has_more = filtered.len() > UI_UNMATCHED_PAGE_LIMIT;
    let page: Vec<_> = filtered.into_iter().take(UI_UNMATCHED_PAGE_LIMIT).collect();

    let next_url = if has_more {
        page.last().map(|e| {
            let mut parts: Vec<(String, String)> = Vec::new();
            if let Some(m) = &method {
                parts.push(("method".into(), m.clone()));
            }
            if let Some(p) = &path_pattern {
                parts.push(("path_pattern".into(), p.clone()));
            }
            parts.push(("before".into(), e.number.to_string()));
            format!("/__ui/unmatched?{}", encode_query(&parts))
        })
    } else {
        None
    };

    let rows: Vec<UnmatchedRow> = page.iter().map(UnmatchedRow::from_record).collect();
    let showing = rows.len() as u64;

    render(
        &state,
        "unmatched_list.html",
        context! {
            page_title => "Unmatched requests",
            user => UserBadge::from(&auth),
            entries => rows,
            showing => showing,
            methods => ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
            filters => UnmatchedFilterState {
                method: method.unwrap_or_default(),
                path_pattern: path_pattern.unwrap_or_default(),
                any_active: any_filter,
            },
            pagination => UnmatchedPagination { next_url },
        },
    )
}

async fn unmatched_detail_page(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(number): Path<u64>,
) -> Response {
    let record = match state.journal().get_unmatched(number) {
        Ok(r) => r,
        Err(_) => return ui_not_found(&state, &auth, &format!("Unmatched #{number}")),
    };
    // ADR-0030 SemFLIP: owner-or-admin of the group it was addressed to.
    let owns = auth.is_admin
        || matches!(
            state.routes().registry().read_group_by_ref(&record.group_id),
            Ok(g) if g.owner_id == auth.user_id
        );
    if !owns {
        return forbidden_page(&state, &auth);
    }
    let view = UnmatchedDetailView::from_record(&record);
    render(
        &state,
        "unmatched_detail.html",
        context! {
            page_title => format!("Unmatched #{}", number),
            user => UserBadge::from(&auth),
            entry => view,
        },
    )
}

#[derive(Serialize)]
struct UnmatchedRow {
    number: u64,
    group: String,
    method: String,
    path: String,
    created_at_iso: String,
    created_at_short: String,
    method_q: String,
    path_q: String,
    /// First near-miss to display inline as "Did you mean…". `None`
    /// when the journal record has no near-misses. The detail page
    /// surfaces the full list.
    primary_hint: Option<UnmatchedRowHint>,
}

#[derive(Serialize)]
struct UnmatchedRowHint {
    route: String,
    route_path: String,
    route_methods: String,
}

impl UnmatchedRow {
    fn from_record(r: &crate::journal::UnmatchedRecord) -> Self {
        let primary_hint = r.near_misses.first().map(|nm| UnmatchedRowHint {
            route: nm.route.clone(),
            route_path: nm.route_path.clone(),
            route_methods: nm.route_methods.join(", "),
        });
        Self {
            number: r.number,
            group: r.group_name.clone(),
            method: r.request.method.clone(),
            path: r.request.path.clone(),
            created_at_iso: r.created_at.to_rfc3339(),
            created_at_short: r.created_at.format("%H:%M:%S").to_string(),
            method_q: urlencoding::encode(&r.request.method).into_owned(),
            path_q: urlencoding::encode(&r.request.path).into_owned(),
            primary_hint,
        }
    }
}

#[derive(Serialize)]
struct UnmatchedFilterState {
    method: String,
    path_pattern: String,
    any_active: bool,
}

#[derive(Serialize)]
struct UnmatchedPagination {
    next_url: Option<String>,
}

#[derive(Serialize)]
struct UnmatchedDetailView {
    id: String,
    number: u64,
    group: String,
    method: String,
    path: String,
    created_at_iso: String,
    trace_id: Option<String>,
    request_headers: Vec<(String, String)>,
    request_body_text: Option<String>,
    request_body_truncated: bool,
    request_body_original_size: usize,
    method_q: String,
    path_q: String,
    near_misses: Vec<UnmatchedDetailNearMiss>,
}

#[derive(Serialize)]
struct UnmatchedDetailNearMiss {
    route: String,
    route_path: String,
    route_methods: String,
    reason: &'static str,
    /// Human-readable detail, e.g. "expected POST, got GET" /
    /// "expected `refunds`, got `refund`".
    explanation: String,
}

impl UnmatchedDetailView {
    fn from_record(r: &crate::journal::UnmatchedRecord) -> Self {
        let near_misses = r
            .near_misses
            .iter()
            .map(UnmatchedDetailNearMiss::from)
            .collect();
        Self {
            id: r.id.clone(),
            number: r.number,
            group: r.group_name.clone(),
            method: r.request.method.clone(),
            path: r.request.path.clone(),
            created_at_iso: r.created_at.to_rfc3339(),
            trace_id: r.trace_id.clone(),
            request_headers: r.request.headers.clone(),
            request_body_text: body_as_text(&r.request.body),
            request_body_truncated: r.request.body_truncated,
            request_body_original_size: r.request.original_body_size,
            method_q: urlencoding::encode(&r.request.method).into_owned(),
            path_q: urlencoding::encode(&r.request.path).into_owned(),
            near_misses,
        }
    }
}

impl From<&crate::journal::UnmatchedNearMiss> for UnmatchedDetailNearMiss {
    fn from(nm: &crate::journal::UnmatchedNearMiss) -> Self {
        let (reason, explanation) = match &nm.reason {
            crate::journal::UnmatchedNearMissReason::MethodMismatch {
                expected_methods,
                got,
            } => (
                "method_mismatch",
                format!(
                    "Pattern matched, but expected {} — got {}.",
                    expected_methods.join(", "),
                    got,
                ),
            ),
            crate::journal::UnmatchedNearMissReason::PrefixMatch { expected, got, .. } => (
                "prefix_match",
                format!("Path differs by one segment: expected `{expected}`, got `{got}`."),
            ),
        };
        Self {
            route: nm.route.clone(),
            route_path: nm.route_path.clone(),
            route_methods: nm.route_methods.join(", "),
            reason,
            explanation,
        }
    }
}

// -- Route creation form (slice 29) -----------------------------------------
//
// A minimal browser-driven create-route flow. Shares the create
// pipeline with `POST /__api/routes` via `api::create_route_core` so
// validation + compile semantics stay identical. UI form is
// source-only (TypeScript / JavaScript) — pre-compiled wasm uploads
// stay on the REST surface where a file-bytes body makes sense.

#[derive(Deserialize)]
struct UiRouteNewQuery {
    method: Option<String>,
    path: Option<String>,
    group: Option<String>,
}

#[derive(Deserialize)]
struct UiRouteNewForm {
    _csrf: String,
    method: String,
    path: String,
    #[serde(default)]
    group: String,
    language: String,
    source: String,
}

#[derive(Serialize, Default, Clone)]
struct RouteNewFormState {
    method: String,
    path: String,
    group: String,
    language: String,
    source: String,
}

// ADR-0020 slice B: the shared js-engine evaluates user source as a
// script (`new Function(source + "; return handle;")`), so the
// top-level declaration must be `function handle(...)` — not
// `export default ...` or any module-shape variant. The in-host swc
// strip pass rewrites `export function handle` → `function handle`,
// but it can't reach `export default async function handle`.
const DEFAULT_TS_HANDLER_SOURCE: &str = "function handle(req, route, group) {\n  return {\n    status: 200,\n    headers: [[\"content-type\", \"application/json\"]],\n    body: new TextEncoder().encode(\"{}\"),\n  };\n}\n";

async fn route_new_form(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<UiRouteNewQuery>,
) -> Response {
    let form = RouteNewFormState {
        method: nonempty(q.method.as_deref()).unwrap_or_else(|| "POST".into()),
        path: nonempty(q.path.as_deref()).unwrap_or_default(),
        group: nonempty(q.group.as_deref()).unwrap_or_default(),
        language: "typescript".into(),
        source: DEFAULT_TS_HANDLER_SOURCE.into(),
    };
    render_route_new(&state, &auth, form, None)
}

async fn route_new_submit(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::Form(form): axum::Form<UiRouteNewForm>,
) -> Response {
    let _ = form._csrf; // already validated by csrf_middleware

    let form_state = RouteNewFormState {
        method: form.method.clone(),
        path: form.path.clone(),
        group: form.group.clone(),
        language: form.language.clone(),
        source: form.source.clone(),
    };

    let group = nonempty(Some(form.group.as_str()));
    if form.path.trim().is_empty() {
        return render_route_new(
            &state,
            &auth,
            form_state,
            Some(RouteNewError {
                title: "Path required".into(),
                message: "Pick a path like /v1/charges.".into(),
                diagnostics: Vec::new(),
            }),
        );
    }

    let body = crate::api::CreateRouteBody {
        group,
        methods: vec![form.method.clone()],
        path: form.path.clone(),
        language: form.language.clone(),
        source: Some(form.source.clone()),
    };

    match crate::api::create_route_core(&state, &auth, body).await {
        Ok(route) => {
            let location = format!("/__ui/routes/{}/{}", route.group_name, route.number);
            let mut resp = Response::default();
            *resp.status_mut() = StatusCode::SEE_OTHER;
            resp.headers_mut().insert(
                axum::http::header::LOCATION,
                axum::http::HeaderValue::try_from(location).expect("ascii location"),
            );
            resp
        }
        Err(api_err) => {
            let title = match api_err.code() {
                "compile_failed" => "Compile failed".into(),
                "conflict" => "Conflicts with an existing route".into(),
                "not_found" => "Unknown group".into(),
                _ => "Couldn't create route".into(),
            };
            render_route_new(
                &state,
                &auth,
                form_state,
                Some(RouteNewError {
                    title,
                    message: api_err.message().to_string(),
                    diagnostics: api_err.diagnostics().to_vec(),
                }),
            )
        }
    }
}

#[derive(Serialize)]
struct RouteNewError {
    title: String,
    message: String,
    diagnostics: Vec<String>,
}

fn render_route_new(
    state: &AppState,
    auth: &AuthContext,
    form: RouteNewFormState,
    error: Option<RouteNewError>,
) -> Response {
    let registry = state.routes().registry();
    let available_groups: Vec<String> = if auth.is_admin {
        registry.list_groups().unwrap_or_default()
    } else {
        registry
            .list_groups_by_owner(&auth.user_id)
            .unwrap_or_default()
    }
    .into_iter()
    // Implicit single-route groups are auto-generated; offering them
    // in the dropdown is noisy. Users get the "(new implicit group)"
    // option for that.
    .filter(|g| !g.implicit)
    .map(|g| g.name)
    .collect();

    let had_error = error.is_some();
    let mut resp = render(
        state,
        "route_new.html",
        context! {
            page_title => "Create route",
            user => UserBadge::from(auth),
            methods => ["POST", "GET", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "ANY"],
            available_groups => available_groups,
            form => form,
            error => error,
        },
    );
    if had_error {
        *resp.status_mut() = StatusCode::BAD_REQUEST;
    }
    resp
}

// -- Group create form ------------------------------------------------------

#[derive(Deserialize)]
struct UiGroupNewForm {
    _csrf: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    ttl_seconds: String,
    /// Checkbox: present (`on`) when checked, absent when not.
    #[serde(default)]
    sliding_ttl: Option<String>,
}

#[derive(Serialize, Default, Clone)]
struct GroupNewFormState {
    name: String,
    ttl_seconds: String,
    sliding_ttl: bool,
}

#[derive(Serialize)]
struct GroupNewError {
    title: String,
    message: String,
}

async fn group_new_form(State(state): State<AppState>, auth: AuthContext) -> Response {
    // Empty name → the registry auto-assigns a friendly DNS-safe one
    // (ADR-0030); sliding TTL defaults on, matching the registry default.
    let form = GroupNewFormState {
        sliding_ttl: true,
        ..GroupNewFormState::default()
    };
    render_group_new(&state, &auth, form, None)
}

async fn group_new_submit(
    State(state): State<AppState>,
    auth: AuthContext,
    axum::Form(form): axum::Form<UiGroupNewForm>,
) -> Response {
    let _ = form._csrf; // already validated by csrf_middleware
    let sliding = form.sliding_ttl.is_some();
    let form_state = GroupNewFormState {
        name: form.name.clone(),
        ttl_seconds: form.ttl_seconds.clone(),
        sliding_ttl: sliding,
    };

    let ttl_seconds = match form.ttl_seconds.trim() {
        "" => None,
        s => match s.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                return render_group_new(
                    &state,
                    &auth,
                    form_state,
                    Some(GroupNewError {
                        title: "Invalid TTL".into(),
                        message: "TTL must be a whole number of seconds, or blank for the default."
                            .into(),
                    }),
                );
            }
        },
    };

    match state
        .routes()
        .registry()
        .create_group(crate::registry::NewGroup {
            // Empty name → registry auto-assigns a friendly DNS-safe one.
            name: form.name.trim().to_string(),
            owner_id: auth.user_id.clone(),
            ttl_seconds,
            sliding_ttl: Some(sliding),
        }) {
        Ok(group) => {
            let location = format!("/__ui/groups/{}", group.name);
            let mut resp = Response::default();
            *resp.status_mut() = StatusCode::SEE_OTHER;
            resp.headers_mut().insert(
                axum::http::header::LOCATION,
                axum::http::HeaderValue::try_from(location).expect("ascii location"),
            );
            resp
        }
        Err(e) => {
            let title = match &e {
                crate::registry::RegistryError::Conflict(_) => "Name already taken",
                crate::registry::RegistryError::InvalidName(_) => "Invalid group name",
                _ => "Couldn't create group",
            };
            render_group_new(
                &state,
                &auth,
                form_state,
                Some(GroupNewError {
                    title: title.into(),
                    message: e.to_string(),
                }),
            )
        }
    }
}

fn render_group_new(
    state: &AppState,
    auth: &AuthContext,
    form: GroupNewFormState,
    error: Option<GroupNewError>,
) -> Response {
    let had_error = error.is_some();
    let mut resp = render(
        state,
        "groups_new.html",
        context! {
            page_title => "Create group",
            user => UserBadge::from(auth),
            form => form,
            error => error,
        },
    );
    if had_error {
        *resp.status_mut() = StatusCode::BAD_REQUEST;
    }
    resp
}

// -- Stubs ------------------------------------------------------------------
//
// Each stub names what'll eventually live at this URL, plus the
// equivalent API path so the user can poke at the data via the CLI
// or curl until the real page lands. Stubs share a single template
// to keep the code DRY.

async fn stub_settings(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(
        &state,
        &auth,
        "Settings",
        "GET /__api/users + /__api/tokens",
    )
}

async fn stub_admin_health(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(&state, &auth, "Admin health", "GET /__health and /__ready")
}

fn stub(state: &AppState, auth: &AuthContext, title: &str, api_hint: &str) -> Response {
    render(
        state,
        "placeholder.html",
        context! {
            page_title => title,
            api_hint => api_hint,
            user => UserBadge::from(auth),
        },
    )
}

fn forbidden_page(state: &AppState, auth: &AuthContext) -> Response {
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Forbidden",
            api_hint => "admin role required",
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp
}

// -- Render helpers ---------------------------------------------------------

/// Render a template with the current CSRF token automatically merged
/// into the caller's context. Handlers stay focused on their page-
/// specific fields — the logout button in `base.html` and any form-
/// bearing template gets `{{ csrf_token }}` for free.
///
/// minijinja's `context!{}` macro produces an opaque tuple-struct
/// shape that serde can't flatten, so we use minijinja's own spread
/// syntax (`..ctx_value`) to merge the page context with the CSRF
/// field — the macro's special-case for spread handles tuple-struct
/// inputs natively.
/// Render the branded UI 404 page for a path under `/__ui/*` that
/// didn't match a real route. Called from `dispatch_inner`'s
/// reserved-path branch so a human pointing a browser at a typo
/// lands on the app shell rather than a JSON error blob. The
/// `requested_path` value is HTML-escaped by minijinja's auto-escape
/// so a crafted URL can't smuggle script tags into the page.
pub(crate) fn render_not_found(state: &AppState, requested_path: &str) -> Response {
    let mut resp = render(
        state,
        "not_found.html",
        context! {
            page_title => "Page not found",
            requested_path => requested_path,
            // No `user` in scope here — dispatch runs without an
            // AuthContext extractor — so the base layout's user area
            // stays empty (the template guards on `{% if user %}`).
            // That's fine for a 404 page.
            user => Option::<UserBadge>::None,
        },
    );
    *resp.status_mut() = StatusCode::NOT_FOUND;
    resp
}

pub(crate) fn render<S: Serialize>(state: &AppState, template: &str, ctx: S) -> Response {
    let csrf_token = csrf::CURRENT_CSRF
        .try_with(|t| t.clone())
        .unwrap_or_default();
    let inner = minijinja::Value::from_serialize(&ctx);
    let merged = context! {
        csrf_token => csrf_token,
        ..inner
    };
    match state.ui_templates().render(template, &merged) {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            tracing::error!(template, error = %e, "template render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template render failed: {e}"),
            )
                .into_response()
        }
    }
}

fn ui_error_500(state: &AppState, auth: &AuthContext, msg: String) -> Response {
    tracing::error!("ui internal error: {msg}");
    let mut resp = render(
        state,
        "placeholder.html",
        context! {
            page_title => "Something went wrong",
            api_hint => msg,
            user => UserBadge::from(auth),
        },
    );
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp
}
