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
        tmpl!("group_detail.html", "templates/group_detail.html");
        tmpl!("routes_list.html", "templates/routes_list.html");
        tmpl!("route_detail.html", "templates/route_detail.html");
        tmpl!("live_journal.html", "templates/live_journal.html");
        tmpl!("journal_entry.html", "templates/journal_entry.html");
        tmpl!("tokens.html", "templates/tokens.html");
        tmpl!("not_found.html", "templates/not_found.html");
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
        .route("/__ui/groups/{group}/state", get(stub_group_state))
        .route("/__ui/routes", get(routes_list_page))
        .route("/__ui/routes/new", get(stub_routes_new))
        .route("/__ui/routes/{group}/{number}", get(route_detail_page))
        .route(
            "/__ui/routes/{group}/{number}/delete",
            axum::routing::post(route_delete_form),
        )
        .route("/__ui/routes/{group}/{number}/state", get(stub_route_state))
        .route("/__ui/journal/live", get(live_journal_page))
        .route("/__ui/journal/{group}/{number}", get(journal_entry_page))
        .route("/__ui/unmatched", get(stub_unmatched))
        .route("/__ui/me/tokens", get(tokens_page).post(create_token_form))
        .route(
            "/__ui/me/tokens/{name}/revoke",
            axum::routing::post(revoke_token_form),
        )
        .route("/__ui/settings", get(stub_settings))
        .route("/__ui/admin/health", get(stub_admin_health))
        .layer(middleware::from_fn(csrf::csrf_middleware))
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
    axum::response::Redirect::to(&format!("/__ui/groups/{}", group.name)).into_response()
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
        language: route.language.clone(),
        bindings_version: route.bindings_version.clone(),
        component_size_human: human_size(route.compiled_wasm.len()),
        owner_name,
        hits_total: route.hits_total,
        last_hit_at: route.last_hit_at.map(|t| t.to_rfc3339()),
        created_at: route.created_at.to_rfc3339(),
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
    language: String,
    bindings_version: String,
    component_size_human: String,
    owner_name: String,
    hits_total: u64,
    last_hit_at: Option<String>,
    created_at: String,
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
    ttl_hours: Option<String>,
    /// Validated by the CSRF middleware; ignored here, only present so
    /// `axum::Form` doesn't reject the request as malformed.
    #[serde(rename = "_csrf")]
    _csrf: String,
}

#[derive(serde::Deserialize)]
struct CsrfOnlyForm {
    #[serde(rename = "_csrf")]
    _csrf: String,
}

async fn tokens_page(State(state): State<AppState>, auth: AuthContext) -> Response {
    let tokens = match state.auth().list_tokens_for(&auth.user_id) {
        Ok(mut ts) => {
            ts.sort_by_key(|t| std::cmp::Reverse(t.created_at));
            ts
        }
        Err(e) => return ui_error_500(&state, &auth, format!("list tokens: {e}")),
    };
    render_tokens_page(&state, &auth, TokensPageData::list(&tokens))
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
    let ttl = match form.ttl_hours.as_deref().map(str::trim) {
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
    render_tokens_page(
        &state,
        &auth,
        TokensPageData::after_create(&tokens, &plaintext, name),
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

#[derive(Serialize)]
struct TokensPageData {
    tokens: Vec<TokenRow>,
    plaintext: Option<String>,
    plaintext_name: Option<String>,
    error: Option<String>,
}

impl TokensPageData {
    fn list(tokens: &[crate::auth::Token]) -> Self {
        Self {
            tokens: tokens.iter().map(TokenRow::from).collect(),
            plaintext: None,
            plaintext_name: None,
            error: None,
        }
    }
    fn after_create(tokens: &[crate::auth::Token], plaintext: &str, name: &str) -> Self {
        Self {
            tokens: tokens.iter().map(TokenRow::from).collect(),
            plaintext: Some(plaintext.to_string()),
            plaintext_name: Some(name.to_string()),
            error: None,
        }
    }
    fn with_error(tokens: &[crate::auth::Token], msg: &str) -> Self {
        Self {
            tokens: tokens.iter().map(TokenRow::from).collect(),
            plaintext: None,
            plaintext_name: None,
            error: Some(msg.to_string()),
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
    let data = TokensPageData::with_error(&tokens, error);
    let mut resp = render_tokens_page(state, auth, data);
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

// -- Stubs ------------------------------------------------------------------
//
// Each stub names what'll eventually live at this URL, plus the
// equivalent API path so the user can poke at the data via the CLI
// or curl until the real page lands. Stubs share a single template
// to keep the code DRY.

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

async fn stub_unmatched(State(state): State<AppState>, auth: AuthContext) -> Response {
    if !auth.is_admin {
        return forbidden_page(&state, &auth);
    }
    stub(&state, &auth, "Unmatched", "GET /__api/unmatched")
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
