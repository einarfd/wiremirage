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
//! Cross-replica note: `refresh_after_group_cascade` updates this
//! host's route-table cache *and* publishes an invalidation, so
//! siblings drop the reaped routes too (ADR-0037). Keyspace
//! notifications were considered for this and rejected — they need
//! server configuration that is off by default and may not be settable
//! on a managed Valkey, and they couple invalidation to physical key
//! names rather than to an application event we control and can
//! version.
//!
//! The interval, plus the per-sweep success/failure logs, are good enough
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
                // One replica sweeps per tick; the rest skip.
                if !tokio::task::block_in_place(|| self.claim_sweep()) {
                    tracing::debug!("another replica holds the sweep lease this tick");
                    continue;
                }
                let outcome = tokio::task::block_in_place(|| {
                    let r = self.single_pass();
                    // Release on success *and* error: a failed pass
                    // holding the lease would stall the next tick too.
                    self.release_sweep();
                    r
                });
                match outcome {
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

    /// Try to claim the sweep for this tick (ADR-0037 item 5).
    ///
    /// Every replica runs a sweeper and every operation a sweep
    /// performs is idempotent, so racing sweepers converge harmlessly —
    /// this is a cost reduction, not a correctness fix. A lease rather
    /// than a lock precisely because of that: whoever loses simply
    /// skips this round, and if the holder dies mid-sweep the lease
    /// expires and the next tick proceeds. Nothing waits, nothing needs
    /// releasing, and a stuck lease cannot wedge the sweeper.
    ///
    /// The lease TTL is a crash guard, not the cadence: it is released
    /// as soon as the pass finishes. Leaving it to expire instead — at
    /// any TTL at or above the interval — means every replica finds it
    /// still held on the next tick and the cluster sweeps on alternating
    /// ticks, at half the configured rate, single-replica deployments
    /// included.
    fn claim_sweep(&self) -> bool {
        // Still generous, because it only has to outlive a slow sweep so
        // a sibling cannot start a second one on top of it.
        let ttl = self.interval.as_secs().max(1) * 2;
        let mut bucket = match self.routes.registry().storage().admin_bucket() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "sweep lease: opening bucket");
                // Fall through to sweeping: duplicated work is the
                // benign outcome, a silently skipped sweep is not.
                return true;
            }
        };
        match bucket.try_acquire_lease("sweep:lease", ttl) {
            Ok(got) => got,
            Err(e) => {
                tracing::warn!(error = %e, "sweep lease: acquire failed");
                true
            }
        }
    }

    /// Release the sweep lease so the next tick can claim it.
    ///
    /// Benign race: a pass that outlives the TTL may delete a lease a
    /// sibling has since taken, letting two replicas sweep in one tick.
    /// Every operation a sweep performs is idempotent (ADR-0037 item 5
    /// and this module's docs), so the cost is duplicated work on a rare
    /// path — cheaper than the fencing token that would prevent it.
    fn release_sweep(&self) {
        let Ok(mut bucket) = self.routes.registry().storage().admin_bucket() else {
            return;
        };
        if let Err(e) = bucket.delete("sweep:lease") {
            // The TTL is the backstop, so this costs at most one skipped
            // tick rather than wedging the sweeper.
            tracing::warn!(error = %e, "releasing sweep lease");
        }
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
    #[test]
    fn the_lease_does_not_cost_every_other_sweep() {
        // The lease is a crash guard, not the cadence. Left to expire on
        // its own it outlives the interval, so every replica finds it
        // held on the next tick and the cluster sweeps at half rate —
        // on single-replica and in-memory deployments too, since every
        // backend takes this path.
        let table = fresh_route_table();
        let sweeper = Sweeper::new(table).with_interval(Duration::from_millis(50));

        for tick in 1..=3 {
            assert!(
                sweeper.claim_sweep(),
                "tick {tick} must claim the lease; a released lease is \
                 immediately re-claimable by the next tick"
            );
            sweeper.release_sweep();
        }
    }

    #[test]
    fn an_unreleased_lease_blocks_a_sibling() {
        // The other half: while a pass is still running, a sibling must
        // not start a second sweep on top of it.
        let table = fresh_route_table();
        let sweeper = Sweeper::new(table).with_interval(Duration::from_millis(50));

        assert!(sweeper.claim_sweep());
        assert!(
            !sweeper.claim_sweep(),
            "held for the duration of the pass, not released between ticks"
        );
        sweeper.release_sweep();
        assert!(sweeper.claim_sweep(), "and claimable once released");
    }

    fn wipe_group_record(registry: &Registry, group_id: &str, group_name: &str) {
        // A Valkey TTL firing drops the whole hash key, not individual
        // fields — delete the key outright so the simulation stays accurate
        // as the group record grows new fields.
        let mut bucket = registry.admin_bucket_for_test();
        bucket.delete(&format!("group:{group_id}")).unwrap();
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
                source: None,
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
                source: None,
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
                source: None,
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
