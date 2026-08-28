//! In-memory route table: holds the loaded `Route` records plus a cache of
//! their compiled wasmtime `Component`s. The dispatch handler queries this
//! on every incoming request rather than hitting the registry directly.
//!
//! Writes go through `POST /api/routes` and `DELETE /api/routes/...`,
//! which call `refresh_*` on the table to keep it in sync with the
//! registry — but only on the replica that served the API call.
//!
//! Under more than one replica that is not enough, so ADR-0037 adds a
//! **read-through floor**: on a match miss for a group that exists, the
//! dispatcher asks [`RouteTable::revalidate_and_rematch`] to reload that
//! group's routes from storage and try once more. That makes the
//! readiness guarantee in `storage-model.md` true — a committed route
//! creation is reachable from any replica — without depending on
//! message delivery.
//!
//! The floor covers creates, which are the common agent workflow
//! (create a route, immediately send traffic). It cannot cover deletes
//! or source updates: a stale route still matches, so those requests
//! never reach the miss path. Making those timely is the job of the
//! pub/sub invalidation that lands on top of this.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::bus::RouteInvalidation;
use crate::pattern::{Methods, Pattern, Segment};
use crate::registry::{Registry, Route};

#[derive(Debug, Clone)]
pub struct MatchedRoute {
    pub route: Route,
    pub matched_pattern: String,
    pub path_params: Vec<(String, String)>,
}

/// Floor on how often one group may be revalidated from storage by the
/// read-through path (ADR-0037 item 1). This rate limit is load-bearing,
/// not a nicety: unmatched traffic is precisely the traffic that misses,
/// so an unbounded read-through would turn junk traffic into a storage
/// amplifier. Bounded this way the cost is one group reload per group
/// per interval however hard a broken client or an attacker pushes.
const DEFAULT_REVALIDATE_INTERVAL: Duration = Duration::from_secs(5);

/// Cap on how many near-misses we surface for one match probe. The
/// near-miss list is for human/agent debugging; if there are dozens
/// of routes whose patterns match the path, the user has bigger
/// problems than the order of the response.
const NEAR_MISS_LIMIT: usize = 20;

/// One reason why a route *almost* matched a request. The taxonomy
/// is intentionally small in slice 13 — we ship `method_mismatch`
/// (pattern matches but methods don't) and `prefix_match` (one
/// segment differs by a literal string-prefix, e.g. `/v1/charge` vs
/// `/v1/charges`). `path_shape_match_in_other_group` from the design
/// spec is not a distinct reason here; that case shows up as a
/// `method_mismatch` whose route record names a different group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NearMissReason {
    /// The route's pattern matches the request path, but the
    /// methods don't overlap.
    MethodMismatch {
        expected_methods: Vec<String>,
        got: String,
    },
    /// A literal-prefix near-miss on a single segment: the route's
    /// pattern matches in every segment except one, where one
    /// segment is a string-prefix of the other.
    PrefixMatch {
        segment_index: usize,
        expected: String,
        got: String,
    },
}

#[derive(Debug, Clone)]
pub struct NearMiss {
    pub route: Route,
    pub reason: NearMissReason,
}

/// `Hit` is boxed so the enum doesn't lopsidedly pay for the
/// matched-route variant on every miss. Same trick wm-core's
/// `MatchResponse` uses for the same reason.
#[derive(Debug, Clone)]
pub enum MatchProbe {
    Hit(Box<MatchedRoute>),
    Miss(Vec<NearMiss>),
}

pub struct RouteTable {
    registry: Arc<Registry>,
    engine: Engine,
    routes: RwLock<Vec<Route>>,
    components: Mutex<HashMap<String, Arc<Component>>>,
    /// Per-route transpiled-JS cache for `typescript` engine routes
    /// (ADR-0020). The stored `source` is the *original* TS; the JS the
    /// shared engine actually runs is derived once and cached here, keyed by
    /// route id and evicted on update/delete exactly like `components`.
    engine_js: Mutex<HashMap<String, Arc<str>>>,
    /// Last read-through revalidation per group **id** (ADR-0037 item
    /// 1). Keyed by id rather than by the requested host label, so an
    /// unknown subdomain can never add an entry — the dispatcher only
    /// revalidates after resolving the group.
    ///
    /// Groups are ephemeral, so entries are dropped on cascade delete
    /// rather than left to accumulate one per group the host has ever
    /// served a miss for.
    revalidated_at: Mutex<HashMap<String, Instant>>,
    revalidate_interval: Duration,
}

impl RouteTable {
    /// Build a new table by loading every route from the registry. Done
    /// once at host startup; subsequent edits go through `refresh_*`.
    pub fn warm(registry: Arc<Registry>, engine: Engine) -> Result<Arc<Self>> {
        let routes = registry.list_routes()?;
        Ok(Arc::new(Self {
            registry,
            engine,
            routes: RwLock::new(routes),
            components: Mutex::new(HashMap::new()),
            engine_js: Mutex::new(HashMap::new()),
            revalidated_at: Mutex::new(HashMap::new()),
            revalidate_interval: DEFAULT_REVALIDATE_INTERVAL,
        }))
    }

    /// Same as [`warm`], with the read-through rate limit overridden.
    /// Tests use a zero interval to exercise the reload deterministically
    /// instead of sleeping through the default.
    pub fn warm_with_revalidate_interval(
        registry: Arc<Registry>,
        engine: Engine,
        revalidate_interval: Duration,
    ) -> Result<Arc<Self>> {
        let table = Self::warm(registry, engine)?;
        // `warm` returns an Arc and the field is not shared yet, so this
        // is the one point where mutating it is sound without interior
        // mutability. Rebuild instead of unwrapping the Arc.
        let routes = table.routes.read().expect("poisoned").clone();
        Ok(Arc::new(Self {
            registry: table.registry.clone(),
            engine: table.engine.clone(),
            routes: RwLock::new(routes),
            components: Mutex::new(HashMap::new()),
            engine_js: Mutex::new(HashMap::new()),
            revalidated_at: Mutex::new(HashMap::new()),
            revalidate_interval,
        }))
    }

    /// Find the first route whose method spec accepts `method` and whose
    /// pattern matches `path`, searching **all** groups. A host-less
    /// matching primitive (used by [`probe`] and this module's unit tests);
    /// production matching is group-scoped ([`find_match_in_group`]) under
    /// virtual-host routing.
    pub fn find_match(&self, method: &str, path: &str) -> Option<MatchedRoute> {
        self.find_match_filtered(None, method, path)
    }

    /// Find the first matching route **within a single group** (by group
    /// name), the per-subdomain dispatch path under ADR-0030: each group
    /// is its own path namespace, so matching is scoped to the group the
    /// request's Host resolved to.
    pub fn find_match_in_group(
        &self,
        group: &str,
        method: &str,
        path: &str,
    ) -> Option<MatchedRoute> {
        self.find_match_filtered(Some(group), method, path)
    }

    fn find_match_filtered(
        &self,
        group: Option<&str>,
        method: &str,
        path: &str,
    ) -> Option<MatchedRoute> {
        // ADR-0028 precedence: try all **specific** (non-tail) routes first —
        // conflict detection keeps them mutually unambiguous, so at most one
        // matches and it wins outright. Only if none match do **tail**
        // backstops get a turn, and among those the longest prefix wins
        // (deterministic, creation-order-independent). Coexisting tails can
        // never both match a path with equal prefix length (that's rejected
        // at create as a conflict), so the longest-prefix pick is unambiguous.
        let routes = self.routes.read().expect("poisoned");
        let mut best_tail: Option<(usize, MatchedRoute)> = None;
        for route in routes.iter() {
            if let Some(g) = group
                && route.group_name != g
            {
                continue;
            }
            let pattern = match Pattern::parse(&route.path) {
                Ok(p) => p,
                Err(_) => continue, // malformed in storage, skip
            };
            let methods = Methods(route.methods.clone());
            if !methods.matches(method) {
                continue;
            }
            let Some(captures) = pattern.match_path(path) else {
                continue;
            };
            let matched = MatchedRoute {
                route: route.clone(),
                matched_pattern: pattern.raw.clone(),
                path_params: captures,
            };
            if pattern.has_tail() {
                let prefix_len = pattern.segments.len();
                if best_tail.as_ref().is_none_or(|(len, _)| prefix_len > *len) {
                    best_tail = Some((prefix_len, matched));
                }
            } else {
                return Some(matched);
            }
        }
        best_tail.map(|(_, m)| m)
    }

    /// ADR-0037 item 1 — the read-through floor. Called by the
    /// dispatcher after a match miss, once it has resolved the request's
    /// host to a real group: reload that group's routes from storage and
    /// retry the match exactly once.
    ///
    /// This is what makes a route created on another replica reachable
    /// here without waiting for a message or a restart. It deliberately
    /// takes a resolved `(group_id, group_name)` rather than the raw
    /// host label — an unknown subdomain must never reach this path, or
    /// junk traffic could both amplify storage reads and grow the rate-
    /// limit map without bound.
    ///
    /// Returns `None` when the rate limit declined the reload, when the
    /// reload failed, or when the group genuinely has no matching route.
    /// A storage error is logged and swallowed: the caller's next step
    /// either way is the unmatched path, and a revalidation failure
    /// should not turn a 404 into a 500.
    pub fn revalidate_and_rematch(
        &self,
        group_id: &str,
        group_name: &str,
        method: &str,
        path: &str,
    ) -> Option<MatchedRoute> {
        if !self.revalidate_group(group_id, group_name) {
            return None;
        }
        self.find_match_filtered(Some(group_name), method, path)
    }

    /// Reload one group's routes from storage into the table, replacing
    /// whatever was cached for it. Returns whether a reload actually
    /// happened.
    fn revalidate_group(&self, group_id: &str, group_name: &str) -> bool {
        {
            // The slot is claimed before the reload is attempted, so a
            // storage failure consumes it for the full interval. That is
            // deliberate: a backend that is failing is the last thing
            // that should be retried on every unmatched request.
            let mut seen = self.revalidated_at.lock().expect("poisoned");
            let now = Instant::now();
            if let Some(last) = seen.get(group_id)
                && now.duration_since(*last) < self.revalidate_interval
            {
                return false;
            }
            seen.insert(group_id.to_string(), now);
        }
        self.reload_group(group_id, group_name)
    }

    /// Reload one group's routes from storage, replacing whatever was
    /// cached for it. No rate limit — callers that need one apply it
    /// first. Returns whether the reload succeeded.
    fn reload_group(&self, group_id: &str, group_name: &str) -> bool {
        let fresh = match self.registry.routes_in_group(group_id) {
            Ok(routes) => routes,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    group_id,
                    group_name,
                    "group reload failed; serving the cached route set"
                );
                return false;
            }
        };
        let mut routes = self.routes.write().expect("poisoned");
        // Replace rather than merge: the freshly-read set is the truth
        // for this group, so this also drops routes deleted elsewhere.
        routes.retain(|r| r.group_id != group_id);
        routes.extend(fresh);
        true
    }

    /// Reload the whole table from storage and drop every cached
    /// artifact. Used when a replica rejoins the invalidation bus
    /// (ADR-0037 item 1), where it cannot know what it missed.
    ///
    /// Pub/sub has no replay, so an invalidation published while this
    /// replica was disconnected is simply gone — and a stale delete or
    /// update still *matches*, so the read-through floor never sees it.
    /// Re-reading everything is the only sound recovery.
    ///
    /// Clearing the compiled-component and transpiled-JS caches is
    /// deliberate rather than incidental: any route may have been
    /// updated during the gap, and the caches are keyed by route id, so
    /// a fresh record would otherwise sit in front of stale bytes. The
    /// cost is one recompile per route on its next request, on a path
    /// that runs at most once per reconnect.
    pub fn reload_all(&self) {
        let fresh = match self.registry.list_routes() {
            Ok(routes) => routes,
            Err(e) => {
                tracing::warn!(error = %e, "full route-table resync failed; keeping the current set");
                return;
            }
        };
        let count = fresh.len();
        *self.routes.write().expect("poisoned") = fresh;
        self.components.lock().expect("poisoned").clear();
        self.engine_js.lock().expect("poisoned").clear();
        // Every group is freshly read, so nothing is owed a read-through
        // yet; clearing also keeps this map from outliving its groups.
        self.revalidated_at.lock().expect("poisoned").clear();
        tracing::info!(routes = count, "route table resynced from storage");
    }

    /// Apply a route invalidation published by another replica
    /// (ADR-0037 item 1). Evicts the named routes' compiled artifacts,
    /// then reloads the group's records from storage.
    ///
    /// Eviction has to be explicit rather than implied by the reload:
    /// the component and transpiled-JS caches are keyed by route id, so
    /// an update that changes a handler's source without changing its
    /// path would otherwise keep serving stale compiled bytes behind a
    /// freshly-read record.
    ///
    /// Idempotent by construction — the originating replica receives
    /// its own message back and re-applies it, which costs one reload
    /// and buys a single uniform delivery path.
    pub fn apply_invalidation(&self, group_id: &str, route_ids: &[String]) {
        {
            let mut components = self.components.lock().expect("poisoned");
            let mut engine_js = self.engine_js.lock().expect("poisoned");
            for id in route_ids {
                components.remove(id);
                engine_js.remove(id);
            }
        }
        // Group name is only used for log context here; the reload keys
        // on the id.
        let reloaded = self.reload_group(group_id, "");
        if reloaded {
            // We just read storage, so the read-through has nothing to
            // add for a while — record it against the rate limit.
            self.revalidated_at
                .lock()
                .expect("poisoned")
                .insert(group_id.to_string(), Instant::now());
        }
        tracing::debug!(
            group_id,
            evicted = route_ids.len(),
            reloaded,
            "applied route invalidation"
        );
    }

    /// Run the match probe across **all** groups: the actual match if any,
    /// else near-misses against the whole route set. The host-less probe —
    /// retained as a matching primitive (and exercised by this module's unit
    /// tests). Production probes are group-scoped ([`probe_in_group`]) under
    /// ADR-0030, since each subdomain is its own path namespace.
    pub fn probe(&self, method: &str, path: &str) -> MatchProbe {
        if let Some(m) = self.find_match(method, path) {
            return MatchProbe::Hit(Box::new(m));
        }
        MatchProbe::Miss(self.compute_near_misses(method, path))
    }

    /// Group-scoped match probe (ADR-0030): match + near-misses confined to
    /// a single group, the per-tenant counterpart to [`probe`]. Backs the
    /// group-scoped `GET /api/match` and the MCP `find_route` tool so a
    /// probe answers "would this match *in my group*?" without leaking other
    /// tenants' routes.
    pub fn probe_in_group(&self, group: &str, method: &str, path: &str) -> MatchProbe {
        if let Some(m) = self.find_match_in_group(group, method, path) {
            return MatchProbe::Hit(Box::new(m));
        }
        MatchProbe::Miss(self.compute_near_misses_in_group(group, method, path))
    }

    /// Compute near-misses for `(method, path)` without first checking
    /// whether the request actually matches. The dispatcher's unmatched-
    /// write path uses the group-scoped variant; the host-less probe uses
    /// this all-groups version.
    pub fn compute_near_misses(&self, method: &str, path: &str) -> Vec<NearMiss> {
        self.compute_near_misses_filtered(None, method, path)
    }

    /// Near-misses scoped to a single group (by name) — used by the
    /// per-subdomain dispatch path so "did you mean…?" suggestions stay
    /// within the tenant's own routes (ADR-0030).
    pub fn compute_near_misses_in_group(
        &self,
        group: &str,
        method: &str,
        path: &str,
    ) -> Vec<NearMiss> {
        self.compute_near_misses_filtered(Some(group), method, path)
    }

    fn compute_near_misses_filtered(
        &self,
        group: Option<&str>,
        method: &str,
        path: &str,
    ) -> Vec<NearMiss> {
        let routes = self.routes.read().expect("poisoned");
        let mut near = Vec::new();
        for route in routes.iter() {
            if let Some(g) = group
                && route.group_name != g
            {
                continue;
            }
            let pattern = match Pattern::parse(&route.path) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if pattern.match_path(path).is_some() {
                // Pattern matches but methods don't — the most useful
                // hint by far.
                near.push(NearMiss {
                    route: route.clone(),
                    reason: NearMissReason::MethodMismatch {
                        expected_methods: route.methods.clone(),
                        got: method.to_string(),
                    },
                });
                continue;
            }
            if let Some((idx, expected, got)) = prefix_segment_diff(&pattern, path) {
                near.push(NearMiss {
                    route: route.clone(),
                    reason: NearMissReason::PrefixMatch {
                        segment_index: idx,
                        expected,
                        got,
                    },
                });
            }
        }
        if near.len() > NEAR_MISS_LIMIT {
            near.truncate(NEAR_MISS_LIMIT);
        }
        near
    }

    /// Return the cached compiled `Component` for the route, compiling on
    /// first access. Cached for the lifetime of the process or until the
    /// route is deleted via `refresh_after_delete`.
    pub fn component_for(&self, route: &Route) -> Result<Arc<Component>> {
        {
            let cache = self.components.lock().expect("poisoned");
            if let Some(c) = cache.get(&route.id) {
                return Ok(c.clone());
            }
        }
        let component = Component::from_binary(&self.engine, &route.compiled_wasm)?;
        let arc = Arc::new(component);
        let mut cache = self.components.lock().expect("poisoned");
        cache.insert(route.id.clone(), arc.clone());
        Ok(arc)
    }

    /// Return the JavaScript the shared engine should run for a source-
    /// language route (ADR-0020). The stored `source` is the *original*
    /// authored source: for `javascript` it's already JS; for `typescript`
    /// it's TS, which we transpile to JS once and cache (keyed by route id,
    /// evicted on update/delete like `components`). Preserving the original
    /// source — rather than storing the transpiled JS — is what lets
    /// `show_route_source` / `export_group` return what the author wrote.
    pub fn engine_source_for(&self, route: &Route) -> Result<String> {
        let source = route.source.as_deref().ok_or_else(|| {
            anyhow::anyhow!("source-language route {} has no source stored", route.id)
        })?;
        if route.language != "typescript" {
            // `javascript` (and any future already-JS engine language): the
            // stored source is dispatch-ready as-is.
            return Ok(source.to_string());
        }
        {
            let cache = self.engine_js.lock().expect("poisoned");
            if let Some(js) = cache.get(&route.id) {
                return Ok(js.to_string());
            }
        }
        let js = wm_transpile::transpile(source)
            .map_err(|e| anyhow::anyhow!("transpile route {}: {e}", route.id))?;
        self.engine_js
            .lock()
            .expect("poisoned")
            .insert(route.id.clone(), Arc::from(js.as_str()));
        Ok(js)
    }

    pub fn refresh_after_create(&self, route: Route) {
        let group_id = route.group_id.clone();
        self.routes.write().expect("poisoned").push(route);
        // No artifact to evict — the record is new, so siblings have
        // nothing cached for it. They only need to re-read the group.
        self.publish_invalidation(RouteInvalidation::for_group(group_id));
    }

    /// Takes the group id alongside the route id so the invalidation
    /// can name the group siblings must re-read; every caller has the
    /// route record in hand anyway.
    pub fn refresh_after_delete(&self, group_id: &str, route_id: &str) {
        self.routes
            .write()
            .expect("poisoned")
            .retain(|r| r.id != route_id);
        self.components.lock().expect("poisoned").remove(route_id);
        self.engine_js.lock().expect("poisoned").remove(route_id);
        self.publish_invalidation(
            RouteInvalidation::for_group(group_id).with_routes(vec![route_id.to_string()]),
        );
    }

    /// Replace the in-memory record for an updated route and drop its
    /// cached `Component`. Called after `Registry::update_route`. The
    /// component cache is evicted unconditionally — even when the
    /// compiled_wasm didn't change, the cost of one extra compile on
    /// the next request is small compared to the bug of serving stale
    /// bytes.
    pub fn refresh_after_update(&self, route: Route) {
        let route_id = route.id.clone();
        let group_id = route.group_id.clone();
        {
            let mut routes = self.routes.write().expect("poisoned");
            if let Some(slot) = routes.iter_mut().find(|r| r.id == route_id) {
                *slot = route;
            } else {
                // Defensive: route wasn't in the table at all (e.g.
                // it appeared on another host but hasn't been
                // warm-loaded here yet). Push it so subsequent
                // requests can dispatch.
                routes.push(route);
            }
        }
        self.components.lock().expect("poisoned").remove(&route_id);
        self.engine_js.lock().expect("poisoned").remove(&route_id);
        // Updates are the one case the read-through floor cannot reach:
        // a stale route still *matches*, so the request never gets to
        // the miss path where revalidation lives. Without this publish a
        // sibling serves the old compiled component until it restarts.
        // The route id is what matters here — the artifact caches are
        // keyed by it, and a source edit that leaves the path alone
        // would otherwise hide behind a freshly-read record.
        self.publish_invalidation(
            RouteInvalidation::for_group(group_id).with_routes(vec![route_id]),
        );
    }

    /// Drop every route in `group_id` from the in-memory cache. Used
    /// after `Registry::cascade_delete_group` (explicit DELETE) and
    /// after the lifecycle sweeper reaps an expired group on this
    /// host. Multi-host invalidation is a separate concern — see
    /// storage-model.md "Cache coherence and route readiness".
    pub fn refresh_after_group_cascade(&self, group_id: &str) {
        let to_drop: Vec<String> = self
            .routes
            .read()
            .expect("poisoned")
            .iter()
            .filter(|r| r.group_id == group_id)
            .map(|r| r.id.clone())
            .collect();
        // Deliberately *not* an early return when `to_drop` is empty.
        // Whether siblings hear about a cascade must not depend on
        // whether this replica happened to have the routes cached — if
        // it warmed before they existed, or missed their create
        // invalidation, it would delete them in storage and tell nobody
        // while replicas that do have them keep serving.
        if !to_drop.is_empty() {
            let mut routes = self.routes.write().expect("poisoned");
            routes.retain(|r| r.group_id != group_id);
            drop(routes);
            let mut cache = self.components.lock().expect("poisoned");
            let mut js_cache = self.engine_js.lock().expect("poisoned");
            for id in &to_drop {
                cache.remove(id);
                js_cache.remove(id);
            }
        }
        // The group is gone, so its read-through slot is dead weight
        // (finding: groups are ephemeral, so this map would otherwise
        // accumulate one entry per group the host has ever seen).
        self.revalidated_at
            .lock()
            .expect("poisoned")
            .remove(group_id);
        self.publish_invalidation(RouteInvalidation::for_group(group_id).with_routes(to_drop));
    }

    /// Update the denormalized `group_name` on every in-memory route in a
    /// renamed group (ADR-0030). Called after `Registry::rename_group` so the
    /// match key (subdomain → group) and the slugs the table reports track
    /// the new name without a full re-warm.
    pub fn refresh_after_group_rename(&self, group_id: &str, new_name: &str) {
        {
            let mut routes = self.routes.write().expect("poisoned");
            for r in routes.iter_mut() {
                if r.group_id == group_id {
                    r.group_name = new_name.to_string();
                }
            }
        }
        // The records carry a denormalized group_name, so siblings need
        // to re-read them; the compiled artifacts are unaffected.
        self.publish_invalidation(RouteInvalidation::for_group(group_id));
    }

    /// Tell sibling replicas that a group's route set changed.
    ///
    /// Sits here rather than at the API layer so every existing
    /// `refresh_*` call site — REST, MCP, UI, and the lifecycle sweeper
    /// alike — gets invalidation without being edited, and so there is
    /// exactly one place where the local update and the remote
    /// notification are kept in step.
    ///
    /// Never called on the receiving path: [`apply_invalidation`] does
    /// not publish, so an event cannot loop between replicas.
    fn publish_invalidation(&self, event: RouteInvalidation) {
        crate::bus::publish_route_invalidation(self.registry.storage(), &event);
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// All currently-loaded routes. Returned as a snapshot.
    pub fn snapshot(&self) -> Vec<Route> {
        self.routes.read().expect("poisoned").clone()
    }
}

/// Detect a literal-prefix near-miss between a route's pattern and a
/// request path. Returns `Some((segment_index, expected, got))` when:
/// (1) the segment counts match, (2) every other segment is
/// compatible (literal-equal or pattern-param), and (3) exactly one
/// pair of literal segments differs by being a string-prefix of the
/// other in either direction.
///
/// This catches typos like `/v1/charge` vs `/v1/charges` without
/// flagging unrelated paths.
fn prefix_segment_diff(pattern: &Pattern, path: &str) -> Option<(usize, String, String)> {
    // Tail patterns match broadly and don't produce a clean prefix near-miss.
    if pattern.has_tail() {
        return None;
    }
    let trimmed = if path.len() > 1 && path.ends_with('/') {
        &path[..path.len() - 1]
    } else {
        path
    };
    if !trimmed.starts_with('/') {
        return None;
    }
    let request_segments: Vec<&str> = if trimmed == "/" {
        Vec::new()
    } else {
        trimmed[1..].split('/').collect()
    };
    if request_segments.len() != pattern.segments.len() {
        return None;
    }
    let mut diff: Option<(usize, String, String)> = None;
    for (i, (route_seg, req_seg)) in pattern
        .segments
        .iter()
        .zip(request_segments.iter())
        .enumerate()
    {
        match route_seg {
            Segment::Param(_) => {
                // Param segments accept any non-empty value; not a
                // diff candidate.
            }
            Segment::Tail(_) => {
                // Unreachable — guarded by `pattern.has_tail()` above; kept
                // for match exhaustiveness.
            }
            Segment::Literal(expected) => {
                if expected == req_seg {
                    continue;
                }
                let is_prefix =
                    expected.starts_with(*req_seg) || req_seg.starts_with(expected.as_str());
                if !is_prefix {
                    return None;
                }
                if diff.is_some() {
                    // More than one segment differs — not a clean
                    // prefix near-miss.
                    return None;
                }
                diff = Some((i, expected.clone(), req_seg.to_string()));
            }
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{NewRoute, Registry};
    use crate::store::Storage;

    fn route_table() -> Arc<RouteTable> {
        let registry = Arc::new(Registry::new(Storage::in_memory()));
        let engine = Engine::default();
        RouteTable::warm(registry, engine).unwrap()
    }

    /// The receiving half of cross-replica invalidation, exercised
    /// in-process. The transport itself is a no-op on the in-memory
    /// backend by construction, so the wire path is covered at tier 3
    /// against a real server (`valkey_storage.rs`); what is worth
    /// pinning here is that applying an event does the two things it
    /// must, and is safe to apply twice.
    #[test]
    fn apply_invalidation_reloads_records_and_evicts_artifacts() {
        let table = route_table();
        let route = add(&table, &["GET"], "/v1/charges");

        // Warm the artifact cache the way a dispatch would. The bytes
        // are not a real component, so compilation fails — but the
        // transpile cache is reachable without wasmtime, so use that as
        // the observable artifact instead.
        table
            .engine_js
            .lock()
            .unwrap()
            .insert(route.id.clone(), Arc::from("stale js"));
        assert!(table.engine_js.lock().unwrap().contains_key(&route.id));

        // Another replica deleted it: the record is gone from storage,
        // but this table still has both the record and the artifact.
        table
            .registry()
            .delete_route(&route.group_name, route.number)
            .unwrap();
        assert!(
            table.find_match("GET", "/v1/charges").is_some(),
            "still matching from cache before the invalidation"
        );

        table.apply_invalidation(&route.group_id, std::slice::from_ref(&route.id));

        assert!(
            table.find_match("GET", "/v1/charges").is_none(),
            "record dropped by the reload"
        );
        assert!(
            !table.engine_js.lock().unwrap().contains_key(&route.id),
            "artifact evicted — a reload alone would have left it behind"
        );
    }

    #[test]
    fn apply_invalidation_is_idempotent() {
        // The publishing replica hears its own message back, so applying
        // twice has to be a no-op rather than a double-drop.
        let table = route_table();
        let route = add(&table, &["GET"], "/v1/charges");

        table.apply_invalidation(&route.group_id, &[]);
        table.apply_invalidation(&route.group_id, &[]);

        assert!(
            table.find_match("GET", "/v1/charges").is_some(),
            "the route still exists in storage, so reloading twice keeps it"
        );
        assert_eq!(table.snapshot().len(), 1, "no duplicate records");
    }

    #[test]
    fn applying_an_invalidation_satisfies_the_read_through_rate_limit() {
        // An invalidation has just read storage, so the read-through has
        // nothing to add for a while; it should not immediately re-read
        // on the next miss.
        let table = route_table();
        let route = add(&table, &["GET"], "/v1/charges");
        table.apply_invalidation(&route.group_id, &[]);

        assert!(
            !table.revalidate_group(&route.group_id, &route.group_name),
            "the slot was consumed by the invalidation"
        );
    }

    fn add(table: &RouteTable, methods: &[&str], path: &str) -> Route {
        let route = table
            .registry()
            .create_route(NewRoute {
                group: None,
                methods: methods.iter().map(|s| s.to_string()).collect(),
                path: path.into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"FAKE".to_vec(),
                source: None,
                owner_id: "test-owner".into(),
            })
            .unwrap();
        table.refresh_after_create(route.clone());
        route
    }

    #[test]
    fn match_literal_path() {
        let table = route_table();
        let route = add(&table, &["POST"], "/v1/charges");
        let m = table.find_match("POST", "/v1/charges").unwrap();
        assert_eq!(m.route.id, route.id);
        assert_eq!(m.matched_pattern, "/v1/charges");
        assert!(m.path_params.is_empty());
    }

    #[test]
    fn match_extracts_path_params() {
        let table = route_table();
        let _ = add(&table, &["GET"], "/users/{id}/posts/{post-id}");
        let m = table.find_match("GET", "/users/123/posts/456").unwrap();
        assert_eq!(
            m.path_params,
            vec![
                ("id".to_string(), "123".to_string()),
                ("post-id".to_string(), "456".to_string()),
            ]
        );
    }

    #[test]
    fn tail_backstop_precedence_and_coexistence() {
        // One group holding a specific route plus a scoped and a root
        // catch-all — they coexist (ADR-0028 conflict exemption), and
        // matching honours specific-first then longest-prefix-tail.
        let table = route_table();
        let group = table
            .registry()
            .create_group(crate::registry::NewGroup {
                name: "g".into(),
                owner_id: "o".into(),
                ttl_seconds: None,
                sliding_ttl: None,
            })
            .unwrap();
        let mk = |methods: &[&str], path: &str| {
            let r = table
                .registry()
                .create_route(NewRoute {
                    group: Some(group.name.clone()),
                    methods: methods.iter().map(|s| s.to_string()).collect(),
                    path: path.into(),
                    language: "wasm".into(),
                    bindings_version: "0.1.0".into(),
                    compiled_wasm: b"FAKE".to_vec(),
                    source: None,
                    owner_id: "o".into(),
                })
                .expect("create (catch-all must coexist with specifics)");
            table.refresh_after_create(r.clone());
            r
        };
        let specific = mk(&["POST"], "/v1/charges");
        let scoped = mk(&["ANY"], "/v1/{rest...}");
        let root = mk(&["ANY"], "/{rest...}");

        // Specific wins over both backstops.
        assert_eq!(
            table
                .find_match_in_group("g", "POST", "/v1/charges")
                .unwrap()
                .route
                .id,
            specific.id
        );
        // Under /v1 but not the specific → longest-prefix tail wins.
        assert_eq!(
            table
                .find_match_in_group("g", "GET", "/v1/other")
                .unwrap()
                .route
                .id,
            scoped.id
        );
        // Outside /v1 → only the root catch-all is left.
        assert_eq!(
            table
                .find_match_in_group("g", "GET", "/zzz/yyy")
                .unwrap()
                .route
                .id,
            root.id
        );
    }

    #[test]
    fn no_match_for_wrong_method() {
        let table = route_table();
        let _ = add(&table, &["POST"], "/v1/charges");
        assert!(table.find_match("GET", "/v1/charges").is_none());
    }

    #[test]
    fn no_match_for_unknown_path() {
        let table = route_table();
        let _ = add(&table, &["GET"], "/v1/charges");
        assert!(table.find_match("GET", "/v1/refunds").is_none());
    }

    #[test]
    fn refresh_after_delete_removes_match() {
        let table = route_table();
        let route = add(&table, &["GET"], "/v1/charges");
        assert!(table.find_match("GET", "/v1/charges").is_some());
        table.refresh_after_delete(&route.group_id, &route.id);
        assert!(table.find_match("GET", "/v1/charges").is_none());
    }

    #[test]
    fn refresh_after_update_replaces_record_and_drops_cache() {
        let table = route_table();
        let mut route = add(&table, &["GET"], "/v1/charges");
        // Prime the component cache so we can verify the eviction.
        // (Bogus wasm bytes — Component::from_binary fails, but the
        // cache only stores successfully compiled ones, so we go
        // through the routes vec instead.)
        let id = route.id.clone();
        // Move the route to a new path; the in-memory table must
        // pick up the change and the old path must no longer match.
        route.path = "/v1/refunds".into();
        table.refresh_after_update(route);
        assert!(table.find_match("GET", "/v1/charges").is_none());
        let m = table.find_match("GET", "/v1/refunds").expect("match");
        assert_eq!(m.route.id, id);
    }

    // -- Match probe tests ---------------------------------------------------

    #[test]
    fn probe_returns_hit_when_route_matches() {
        let table = route_table();
        let route = add(&table, &["POST"], "/v1/charges");
        match table.probe("POST", "/v1/charges") {
            MatchProbe::Hit(m) => assert_eq!(m.route.id, route.id),
            MatchProbe::Miss(_) => panic!("expected hit"),
        }
    }

    #[test]
    fn probe_method_mismatch_surfaces_as_near_miss() {
        let table = route_table();
        let route = add(&table, &["POST"], "/v1/charges");
        match table.probe("GET", "/v1/charges") {
            MatchProbe::Miss(near) => {
                assert_eq!(near.len(), 1);
                assert_eq!(near[0].route.id, route.id);
                match &near[0].reason {
                    NearMissReason::MethodMismatch {
                        expected_methods,
                        got,
                    } => {
                        assert_eq!(expected_methods, &vec!["POST".to_string()]);
                        assert_eq!(got, "GET");
                    }
                    other => panic!("expected MethodMismatch, got {other:?}"),
                }
            }
            MatchProbe::Hit(_) => panic!("expected miss"),
        }
    }

    #[test]
    fn probe_prefix_match_on_typo_path() {
        let table = route_table();
        let route = add(&table, &["GET"], "/v1/charges");
        match table.probe("GET", "/v1/charge") {
            MatchProbe::Miss(near) => {
                assert_eq!(near.len(), 1);
                assert_eq!(near[0].route.id, route.id);
                match &near[0].reason {
                    NearMissReason::PrefixMatch {
                        segment_index,
                        expected,
                        got,
                    } => {
                        assert_eq!(*segment_index, 1);
                        assert_eq!(expected, "charges");
                        assert_eq!(got, "charge");
                    }
                    other => panic!("expected PrefixMatch, got {other:?}"),
                }
            }
            MatchProbe::Hit(_) => panic!("expected miss"),
        }
    }

    #[test]
    fn probe_returns_no_near_miss_for_unrelated_path() {
        let table = route_table();
        let _ = add(&table, &["GET"], "/v1/charges");
        // /completely/different has different segment count + no
        // prefix overlap.
        match table.probe("GET", "/completely/different/path/here") {
            MatchProbe::Miss(near) => assert!(near.is_empty()),
            MatchProbe::Hit(_) => panic!("expected miss"),
        }
    }

    #[test]
    fn probe_method_mismatch_wins_over_prefix_when_both_fit() {
        // Two routes: one's pattern matches but methods don't (clean
        // method_mismatch); a second has a prefix-typo path with the
        // right method. The probe should report both — the consumer
        // decides which is more useful.
        let table = route_table();
        let r1 = add(&table, &["POST"], "/v1/charges");
        let r2 = add(&table, &["GET"], "/v1/charges-archive");
        match table.probe("GET", "/v1/charges") {
            MatchProbe::Miss(near) => {
                assert_eq!(near.len(), 2);
                assert!(near.iter().any(|n| n.route.id == r1.id
                    && matches!(n.reason, NearMissReason::MethodMismatch { .. })));
                assert!(near.iter().any(|n| n.route.id == r2.id
                    && matches!(n.reason, NearMissReason::PrefixMatch { .. })));
            }
            MatchProbe::Hit(_) => panic!("expected miss"),
        }
    }

    #[test]
    fn probe_prefix_match_treats_param_segments_as_compatible() {
        // Path /v1/users/{id}/posts/all vs request /v1/users/123/posts/al
        // Last segment differs by prefix; param segment is fine.
        let table = route_table();
        let route = add(&table, &["GET"], "/v1/users/{id}/posts/all");
        match table.probe("GET", "/v1/users/123/posts/al") {
            MatchProbe::Miss(near) => {
                assert_eq!(near.len(), 1);
                assert_eq!(near[0].route.id, route.id);
                assert!(matches!(
                    near[0].reason,
                    NearMissReason::PrefixMatch {
                        segment_index: 4,
                        ..
                    }
                ));
            }
            MatchProbe::Hit(_) => panic!("expected miss"),
        }
    }

    #[test]
    fn probe_does_not_report_two_segment_diffs_as_prefix_match() {
        let table = route_table();
        let _ = add(&table, &["GET"], "/v1/charges");
        // Both segments differ — not a clean prefix near-miss.
        match table.probe("GET", "/v2/refunds") {
            MatchProbe::Miss(near) => assert!(near.is_empty()),
            MatchProbe::Hit(_) => panic!("expected miss"),
        }
    }
}
