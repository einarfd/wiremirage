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
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use minijinja::Environment;
use minijinja::context;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::{GroupsListQuery, RoutesListQuery, list_groups_core, list_routes_core};
use crate::auth::AuthContext;
use crate::registry::{Group, Route};

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
        tmpl!("groups_list.html", "templates/groups_list.html");
        tmpl!("routes_list.html", "templates/routes_list.html");
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
        .route("/__ui/groups", get(groups_list_page))
        .route("/__ui/groups/{group}", get(stub_group_detail))
        .route("/__ui/groups/{group}/state", get(stub_group_state))
        .route("/__ui/routes", get(routes_list_page))
        .route("/__ui/routes/new", get(stub_routes_new))
        .route("/__ui/routes/{group}/{number}", get(stub_route_detail))
        .route("/__ui/routes/{group}/{number}/state", get(stub_route_state))
        .route("/__ui/journal/live", get(stub_journal_live))
        .route("/__ui/journal/{group}/{number}", get(stub_journal_entry))
        .route("/__ui/unmatched", get(stub_unmatched))
        .route("/__ui/me/tokens", get(stub_tokens))
        .route("/__ui/settings", get(stub_settings))
        .route("/__ui/admin/health", get(stub_admin_health))
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

// -- Stubs ------------------------------------------------------------------
//
// Each stub names what'll eventually live at this URL, plus the
// equivalent API path so the user can poke at the data via the CLI
// or curl until the real page lands. Stubs share a single template
// to keep the code DRY.

async fn stub_group_detail(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group): Path<String>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Group: {group}"),
        &format!("GET /__api/groups/{group}"),
    )
}

async fn stub_group_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(group): Path<String>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("State: {group}"),
        // No REST endpoint for group-wide state listing yet; the
        // user can drop to per-route state.
        "DELETE /__api/groups/{group}/state to wipe",
    )
}

async fn stub_routes_new(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(&state, &auth, "New route", "POST /__api/routes")
}

async fn stub_route_detail(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Route: {group}/{number}"),
        &format!("GET /__api/routes/{group}/{number}"),
    )
}

async fn stub_route_state(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Route state: {group}/{number}"),
        &format!("GET /__api/routes/{group}/{number}/state"),
    )
}

async fn stub_journal_live(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(
        &state,
        &auth,
        "Live journal",
        "GET /__api/journal/tail (SSE)",
    )
}

async fn stub_journal_entry(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((group, number)): Path<(String, u32)>,
) -> Response {
    stub(
        &state,
        &auth,
        &format!("Journal: {group}/{number}"),
        &format!("GET /__api/journal/{group}/{number}"),
    )
}

async fn stub_unmatched(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(&state, &auth, "Unmatched", "GET /__api/unmatched")
}

async fn stub_tokens(State(state): State<AppState>, auth: AuthContext) -> Response {
    stub(&state, &auth, "My tokens", "GET /__api/tokens")
}

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

fn render<S: Serialize>(state: &AppState, template: &str, ctx: S) -> Response {
    match state.ui_templates().render(template, ctx) {
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
