//! In-memory route table: holds the loaded `Route` records plus a cache of
//! their compiled wasmtime `Component`s. The dispatch handler queries this
//! on every incoming request rather than hitting the registry directly.
//!
//! Slice 3 is single-host: writes go through `POST /__api/routes` and
//! `DELETE /__api/routes/...`, which call `refresh_*` on the table to keep
//! it in sync with the registry. Multi-host coherence (Valkey keyspace
//! notifications per `storage-model.md`) is a slice 4 concern.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::pattern::{Methods, Pattern};
use crate::registry::{Registry, Route};

#[derive(Debug, Clone)]
pub struct MatchedRoute {
    pub route: Route,
    pub matched_pattern: String,
    pub path_params: Vec<(String, String)>,
}

pub struct RouteTable {
    registry: Arc<Registry>,
    engine: Engine,
    routes: RwLock<Vec<Route>>,
    components: Mutex<HashMap<String, Arc<Component>>>,
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
        }))
    }

    /// Find the first route whose method spec accepts `method` and whose
    /// pattern matches `path`. Slice 3 doesn't promise any particular tie-
    /// breaker — strict conflict detection at create time keeps the route
    /// set unambiguous.
    pub fn find_match(&self, method: &str, path: &str) -> Option<MatchedRoute> {
        let routes = self.routes.read().expect("poisoned");
        for route in routes.iter() {
            let pattern = match Pattern::parse(&route.path) {
                Ok(p) => p,
                Err(_) => continue, // malformed in storage, skip
            };
            let methods = Methods(route.methods.clone());
            if !methods.matches(method) {
                continue;
            }
            if let Some(captures) = pattern.match_path(path) {
                return Some(MatchedRoute {
                    route: route.clone(),
                    matched_pattern: pattern.raw,
                    path_params: captures,
                });
            }
        }
        None
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

    pub fn refresh_after_create(&self, route: Route) {
        self.routes.write().expect("poisoned").push(route);
    }

    pub fn refresh_after_delete(&self, route_id: &str) {
        self.routes
            .write()
            .expect("poisoned")
            .retain(|r| r.id != route_id);
        self.components.lock().expect("poisoned").remove(route_id);
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// All currently-loaded routes. Returned as a snapshot.
    pub fn snapshot(&self) -> Vec<Route> {
        self.routes.read().expect("poisoned").clone()
    }
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
        table.refresh_after_delete(&route.id);
        assert!(table.find_match("GET", "/v1/charges").is_none());
    }
}
