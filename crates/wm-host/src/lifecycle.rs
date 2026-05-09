//! Background lifecycle sweeper.
//!
//! Walks the route table periodically (default every 30s) and, for
//! each route whose parent group no longer exists, cascade-deletes the
//! children and invalidates the local in-memory route table cache.
//!
//! The check is "the route's group_id no longer resolves to a group
//! record". When a group's `EXPIRE` fires in Valkey, the `group:{ulid}`
//! hash and the `group:by-name:{name}` index disappear, so resolution
//! returns `NotFound` and the sweeper acts.
//!
//! Idempotency: every Valkey op the sweeper invokes (`DEL`, `HDEL`,
//! `SREM`, `SCAN+DEL`) is a no-op when its target is already gone, so
//! multiple sweepers racing against the same expired group on
//! different hosts converge harmlessly. Worst case is wasted CPU and
//! a few duplicate Valkey round-trips; correctness is preserved.
//!
//! Multi-host caveat: `refresh_after_group_cascade` only invalidates
//! *this* host's route-table cache. Other hosts in a multi-host
//! deployment still serve stale routes from their own caches until
//! their next sweep pass touches the same group, or until they
//! restart. The proper fix is Valkey keyspace notifications — see
//! [[../storage-model.md]] "Cache coherence and route readiness".
//!
//! Slice 8 scope: sweeper only, no keyspace notifications. The
//! interval, plus the per-sweep success/failure logs, are good enough
//! for "long-running tests don't accumulate orphans" — the original
//! lifecycle correctness goal.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::registry::RegistryError;
use crate::route_table::RouteTable;

/// Default cadence for the sweeper. Per `storage-model.md`'s "TTL
/// strategy" section: every 30s catches expiry in time without
/// hammering the cluster.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Sweeper {
    routes: Arc<RouteTable>,
    interval: Duration,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub groups_swept: u64,
    pub routes_reaped: u64,
}

impl Sweeper {
    pub fn new(routes: Arc<RouteTable>) -> Self {
        Self {
            routes,
            interval: DEFAULT_SWEEP_INTERVAL,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Spawn the sweeper as a long-running tokio task. The returned
    /// handle is held by the caller (typically `main`) so the task
    /// is cleaned up on process shutdown via tokio's drop semantics.
    /// The first sweep happens after one full interval — startup
    /// shouldn't churn the cluster.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.interval);
            // The first call to tick() fires immediately by default.
            // Burn it so the first real sweep is one interval out.
            interval.tick().await;
            loop {
                interval.tick().await;
                match tokio::task::block_in_place(|| self.single_pass()) {
                    Ok(stats) if stats.groups_swept > 0 => {
                        tracing::info!(
                            groups_swept = stats.groups_swept,
                            routes_reaped = stats.routes_reaped,
                            "lifecycle sweep reaped expired groups"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "lifecycle sweep failed");
                    }
                }
            }
        })
    }

    /// One sweep pass, executed synchronously. Public so tests can
    /// drive it deterministically against a fixture state.
    pub fn single_pass(&self) -> Result<SweepStats, RegistryError> {
        let registry = self.routes.registry();
        let routes = registry.list_routes()?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut stats = SweepStats::default();
        for route in routes {
            if !seen.insert(route.group_id.clone()) {
                continue;
            }
            // Probe: does the parent group still exist?
            if registry.read_group_by_ref(&route.group_id).is_ok() {
                continue;
            }
            // Parent is gone — cascade and invalidate the local
            // route-table cache. The cascade is best-effort (already
            // partially-cleaned state is fine).
            let reaped = registry.cascade_delete_group(&route.group_id)?;
            self.routes.refresh_after_group_cascade(&route.group_id);
            stats.groups_swept += 1;
            stats.routes_reaped += reaped;
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registry::{NewRoute, Registry};
    use crate::store::Storage;
    use wasmtime::Engine;

    fn fresh_route_table() -> Arc<RouteTable> {
        let storage = Storage::in_memory();
        let registry = Arc::new(Registry::new(storage));
        let engine = Engine::default();
        RouteTable::warm(registry, engine).unwrap()
    }

    /// Manually wipe the group record from storage to simulate the
    /// Valkey TTL firing. We use the registry's storage handle via a
    /// helper crate of the test.
    fn wipe_group_record(registry: &Registry, group_id: &str, group_name: &str) {
        let mut bucket = registry.admin_bucket_for_test();
        for field in [
            "id",
            "name",
            "implicit",
            "created_at",
            "owner_id",
            "ttl_seconds",
            "sliding_ttl",
        ] {
            bucket
                .hash_delete(&format!("group:{group_id}"), field)
                .unwrap();
        }
        bucket
            .delete(&format!("group:by-name:{group_name}"))
            .unwrap();
    }

    fn add_route(table: &RouteTable, owner_id: &str, path: &str) -> crate::registry::Route {
        let route = table
            .registry()
            .create_route(NewRoute {
                group: None,
                methods: vec!["POST".into()],
                path: path.into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"FAKE".to_vec(),
                owner_id: owner_id.into(),
            })
            .unwrap();
        table.refresh_after_create(route.clone());
        route
    }

    #[test]
    fn single_pass_is_noop_when_groups_alive() {
        let table = fresh_route_table();
        add_route(&table, "alice", "/v1/foo");
        add_route(&table, "alice", "/v1/bar");
        let sweeper = Sweeper::new(table.clone());
        let stats = sweeper.single_pass().unwrap();
        assert_eq!(stats, SweepStats::default());
        // Both routes still present.
        assert_eq!(table.snapshot().len(), 2);
    }

    #[test]
    fn single_pass_reaps_routes_whose_group_vanished() {
        let table = fresh_route_table();
        let r1 = add_route(&table, "alice", "/v1/foo");
        let r2 = add_route(&table, "alice", "/v1/bar");
        // Both routes share an implicit group each (one route per
        // implicit group). Wipe one group's record.
        wipe_group_record(table.registry(), &r1.group_id, &r1.group_name);

        let sweeper = Sweeper::new(table.clone());
        let stats = sweeper.single_pass().unwrap();
        assert_eq!(stats.groups_swept, 1);
        assert_eq!(stats.routes_reaped, 1);

        // r1 is gone from the route table, r2 is still there.
        let snap = table.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, r2.id);
    }

    #[test]
    fn single_pass_dedups_by_group_id() {
        // Create a single named group and two routes inside it; wipe
        // the group's record. The sweeper should cascade once, not
        // twice, even though it sees two orphaned routes.
        let table = fresh_route_table();
        let r1 = table
            .registry()
            .create_route(NewRoute {
                group: None,
                methods: vec!["POST".into()],
                path: "/v1/a".into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"A".to_vec(),
                owner_id: "alice".into(),
            })
            .unwrap();
        let r2 = table
            .registry()
            .create_route(NewRoute {
                group: Some(r1.group_name.clone()),
                methods: vec!["POST".into()],
                path: "/v1/b".into(),
                language: "wasm".into(),
                bindings_version: "0.1.0".into(),
                compiled_wasm: b"B".to_vec(),
                owner_id: "alice".into(),
            })
            .unwrap();
        table.refresh_after_create(r1.clone());
        table.refresh_after_create(r2.clone());
        wipe_group_record(table.registry(), &r1.group_id, &r1.group_name);

        let sweeper = Sweeper::new(table.clone());
        let stats = sweeper.single_pass().unwrap();
        assert_eq!(stats.groups_swept, 1);
        assert_eq!(stats.routes_reaped, 2);
        assert!(table.snapshot().is_empty());
    }

    #[test]
    fn single_pass_is_idempotent_across_runs() {
        let table = fresh_route_table();
        let r1 = add_route(&table, "alice", "/v1/foo");
        wipe_group_record(table.registry(), &r1.group_id, &r1.group_name);

        let sweeper = Sweeper::new(table.clone());
        let first = sweeper.single_pass().unwrap();
        assert_eq!(first.groups_swept, 1);
        let second = sweeper.single_pass().unwrap();
        assert_eq!(second, SweepStats::default());
    }
}
