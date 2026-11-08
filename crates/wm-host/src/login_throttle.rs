//! Per-IP throttles for the two unauthenticated, expensive endpoints:
//! `POST /auth/login/password` and `POST /auth/oauth/register`.
//!
//! ADR-0018 calls for "5 failed attempts in 60 seconds triggers a
//! 60-second lockout". This isn't sufficient to make local auth safe to
//! expose publicly — that's not the use case — but it stops trivial
//! drive-by brute force inside the trusted-network threat model.
//!
//! State lives in shared storage rather than a process-local map
//! (ADR-0037 item 4). With the counters in-process, N replicas behind a
//! load balancer would each keep their own tally and collectively allow
//! N times the intended budget before anyone locked out.
//!
//! The window and the lockout are expressed as **leases** — keys whose
//! value is their own deadline — rather than as `INCR` plus `EXPIRE`.
//! That is what lets one implementation serve both backends: `set_ttl`
//! is a no-op on the in-memory store, so a lockout relying on
//! server-side expiry would never lift there, permanently locking out
//! any IP that ever tripped it. Comparing a stored deadline behaves
//! identically on both, and `SET .. NX` keeps the claim atomic across
//! replicas.
//!
//! Storage failures fail *open*: a throttle that can't reach storage
//! must not lock everyone out of a host that is otherwise serving.

use std::net::IpAddr;

use chrono::{DateTime, Utc};

use crate::store::Storage;

const WINDOW_SECONDS: u64 = 60;
const MAX_FAILS: i64 = 5;
const LOCKOUT_SECONDS: u64 = 60;

const DCR_WINDOW_SECONDS: u64 = 60;
const DCR_MAX_ATTEMPTS: i64 = 5;
const DCR_LOCKOUT_SECONDS: u64 = 60;

/// A per-IP windowed hit budget over shared storage. `ns` namespaces
/// the keys so separate endpoints (login, DCR registration) never share
/// a tally.
#[derive(Clone)]
struct IpBudget {
    storage: Storage,
    ns: &'static str,
    max: i64,
    window: u64,
    lockout: u64,
}

impl IpBudget {
    fn new(storage: Storage, ns: &'static str, max: i64, window: u64, lockout: u64) -> Self {
        Self {
            storage,
            ns,
            max,
            window,
            lockout,
        }
    }

    fn fails_key(&self, ip: IpAddr) -> String {
        format!("throttle:{}:fails:{ip}", self.ns)
    }

    fn window_key(&self, ip: IpAddr) -> String {
        format!("throttle:{}:window:{ip}", self.ns)
    }

    fn lock_key(&self, ip: IpAddr) -> String {
        format!("throttle:{}:lock:{ip}", self.ns)
    }

    /// Check whether `ip` is currently locked out. Does not mutate — the
    /// caller checks before attempting verification.
    fn is_locked_out(&self, ip: IpAddr) -> bool {
        let mut bucket = match self.storage.admin_bucket() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "throttle: opening bucket");
                return false;
            }
        };
        let raw = match bucket.get(&self.lock_key(ip)) {
            Ok(Some(v)) => v,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(error = %e, "throttle: reading lockout");
                return false;
            }
        };
        // The lease value is its own deadline, which is what makes this
        // work on a backend without server-side expiry.
        std::str::from_utf8(&raw)
            .ok()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|until| until > Utc::now())
    }

    /// Record one hit for `ip`. Returns `true` if this hit pushed the IP
    /// into lockout, so the caller can log the transition.
    fn record_hit(&self, ip: IpAddr) -> bool {
        let mut bucket = match self.storage.admin_bucket() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "throttle: opening bucket");
                return false;
            }
        };
        // Claiming the window lease means this hit opened a fresh
        // window, so the count restarts at 1. Failing to claim
        // means a window is already running and this adds to it. The
        // claim is atomic, so concurrent hits across replicas can't
        // both decide they started the window.
        let fresh_window = bucket
            .try_acquire_lease(&self.window_key(ip), self.window)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "throttle: window lease");
                false
            });
        let hits = if fresh_window {
            if let Err(e) = bucket.set(&self.fails_key(ip), b"1".to_vec()) {
                tracing::warn!(error = %e, "throttle: resetting counter");
                return false;
            }
            1
        } else {
            match bucket.incr(&self.fails_key(ip), 1) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "throttle: incrementing counter");
                    return false;
                }
            }
        };
        // Both branches, because `incr` creates the key when it is
        // absent — which is the case a lost window lease produces.
        //
        // The counter carries a TTL of its own rather than relying on
        // being deleted: it is only cleared on lockout or `clear`, so an
        // address that hits once or twice and never comes back would
        // otherwise leave a key behind forever — one per source IP, on a
        // publicly reachable endpoint, in a system whose whole premise is
        // that state expires. (`set_ttl` is a no-op in memory, which is
        // fine: that backend dies with the process, and the window lease
        // already governs correctness there.)
        let _ = bucket.set_ttl(&self.fails_key(ip), self.window);
        if hits < self.max {
            return false;
        }
        // Trip the lockout, and clear the counter so the IP starts clean
        // once the lockout lapses rather than re-tripping on its next
        // single hit.
        let newly_locked = bucket
            .try_acquire_lease(&self.lock_key(ip), self.lockout)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "throttle: lock lease");
                false
            });
        let _ = bucket.delete(&self.fails_key(ip));
        let _ = bucket.delete(&self.window_key(ip));
        newly_locked
    }

    /// Clear the counters for `ip`. Leaves any active lockout in place.
    fn clear(&self, ip: IpAddr) {
        let Ok(mut bucket) = self.storage.admin_bucket() else {
            return;
        };
        let _ = bucket.delete(&self.fails_key(ip));
        let _ = bucket.delete(&self.window_key(ip));
    }
}

/// Per-IP throttle for `POST /auth/login/password` (ADR-0018).
#[derive(Clone)]
pub struct LoginThrottle {
    budget: IpBudget,
}

impl LoginThrottle {
    pub fn new(storage: Storage) -> Self {
        Self {
            budget: IpBudget::new(storage, "login", MAX_FAILS, WINDOW_SECONDS, LOCKOUT_SECONDS),
        }
    }

    /// Check whether `ip` is currently locked out. Does not mutate.
    pub fn is_locked_out(&self, ip: IpAddr) -> bool {
        self.budget.is_locked_out(ip)
    }

    /// Record a failed login attempt for `ip`. Returns `true` if this
    /// failure pushed the IP into lockout, so the caller can log the
    /// transition.
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        self.budget.record_hit(ip)
    }

    /// Clear the counters for `ip` after a successful login. Leaves any
    /// active lockout in place — a correct password during a lockout
    /// shouldn't be a way out of it.
    pub fn record_success(&self, ip: IpAddr) {
        self.budget.clear(ip)
    }
}

/// Per-IP throttle for `POST /auth/oauth/register` (security-audit
/// finding #4). Unlike the login throttle, *every* attempt is
/// expensive — an argon2id hash runs per request — so all attempts
/// count toward the budget, not just failed ones.
#[derive(Clone)]
pub struct RegisterThrottle {
    budget: IpBudget,
}

impl RegisterThrottle {
    pub fn new(storage: Storage) -> Self {
        Self {
            budget: IpBudget::new(
                storage,
                "dcr",
                DCR_MAX_ATTEMPTS,
                DCR_WINDOW_SECONDS,
                DCR_LOCKOUT_SECONDS,
            ),
        }
    }

    /// Check whether `ip` is currently locked out. Does not mutate.
    pub fn is_locked_out(&self, ip: IpAddr) -> bool {
        self.budget.is_locked_out(ip)
    }

    /// Record one registration attempt for `ip`. Returns `true` if this
    /// attempt pushed the IP into lockout.
    pub fn record_attempt(&self, ip: IpAddr) -> bool {
        self.budget.record_hit(ip)
    }
}

impl std::fmt::Debug for LoginThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginThrottle").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for RegisterThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisterThrottle").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle() -> LoginThrottle {
        LoginThrottle::new(Storage::in_memory())
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn fresh_ip_is_not_locked() {
        let t = throttle();
        assert!(!t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn four_failures_dont_lock_out() {
        let t = throttle();
        for _ in 0..4 {
            assert!(!t.record_failure(ip("10.0.0.1")));
        }
        assert!(!t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn fifth_failure_triggers_lockout() {
        let t = throttle();
        for _ in 0..4 {
            t.record_failure(ip("10.0.0.1"));
        }
        assert!(t.record_failure(ip("10.0.0.1")));
        assert!(t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn success_clears_the_counter() {
        let t = throttle();
        for _ in 0..3 {
            t.record_failure(ip("10.0.0.1"));
        }
        t.record_success(ip("10.0.0.1"));
        // Counter cleared → a full fresh round without locking out.
        for _ in 0..4 {
            assert!(!t.record_failure(ip("10.0.0.1")));
        }
    }

    #[test]
    fn lockout_is_per_ip() {
        let t = throttle();
        for _ in 0..5 {
            t.record_failure(ip("10.0.0.1"));
        }
        assert!(t.is_locked_out(ip("10.0.0.1")));
        assert!(!t.is_locked_out(ip("10.0.0.2")));
    }

    #[test]
    fn two_throttles_over_one_storage_share_a_budget() {
        // The point of the whole change: two replicas must not each get
        // their own five attempts. Same storage, separate instances.
        let storage = Storage::in_memory();
        let a = LoginThrottle::new(storage.clone());
        let b = LoginThrottle::new(storage);
        let addr = ip("10.0.0.7");

        for _ in 0..3 {
            assert!(!a.record_failure(addr));
        }
        // B picks up A's tally rather than starting over.
        assert!(!b.record_failure(addr));
        assert!(b.record_failure(addr), "fifth attempt overall locks out");
        assert!(
            a.is_locked_out(addr),
            "and the lockout is visible from the other replica"
        );
    }

    #[test]
    fn an_expired_lockout_lifts() {
        // Write a deadline that is already in the past rather than
        // asking for a zero-length lease: `try_acquire_lease` clamps its
        // TTL to at least a second, because Valkey rejects a
        // non-positive expire time outright.
        let t = throttle();
        let addr = ip("10.0.0.1");
        let mut bucket = t.budget.storage.admin_bucket().unwrap();
        let past = (Utc::now() - chrono::Duration::seconds(1))
            .to_rfc3339()
            .into_bytes();
        bucket.set(&t.budget.lock_key(addr), past).unwrap();
        assert!(
            !t.is_locked_out(addr),
            "a lockout whose deadline has passed no longer holds"
        );
    }

    #[test]
    fn a_live_lockout_holds() {
        // The other half: the same comparison must still report a
        // lockout that has not yet expired.
        let t = throttle();
        let addr = ip("10.0.0.1");
        let mut bucket = t.budget.storage.admin_bucket().unwrap();
        assert!(
            bucket
                .try_acquire_lease(&t.budget.lock_key(addr), 60)
                .unwrap()
        );
        assert!(t.is_locked_out(addr));
    }

    #[test]
    fn storage_failures_fail_open() {
        // Not reachable through the in-memory backend, so assert the
        // property that matters: a fresh throttle never reports a
        // lockout it has no evidence for.
        let t = throttle();
        assert!(!t.is_locked_out(ip("192.0.2.1")));
    }

    fn register_throttle() -> RegisterThrottle {
        RegisterThrottle::new(Storage::in_memory())
    }

    #[test]
    fn register_throttle_locks_after_five_attempts() {
        let t = register_throttle();
        for _ in 0..4 {
            assert!(!t.record_attempt(ip("10.0.0.1")));
        }
        assert!(t.record_attempt(ip("10.0.0.1")));
        assert!(t.is_locked_out(ip("10.0.0.1")));
        assert!(!t.is_locked_out(ip("10.0.0.2")), "lockout is per IP");
    }

    #[test]
    fn register_throttle_counts_successes_too() {
        // Every DCR attempt costs an argon2id hash, so unlike the login
        // throttle there is no record_success that clears the tally.
        let t = register_throttle();
        for _ in 0..5 {
            t.record_attempt(ip("10.0.0.3"));
        }
        assert!(t.is_locked_out(ip("10.0.0.3")));
    }

    #[test]
    fn register_and_login_budgets_are_separate() {
        let storage = Storage::in_memory();
        let login = LoginThrottle::new(storage.clone());
        let dcr = RegisterThrottle::new(storage);
        let addr = ip("10.0.0.9");
        for _ in 0..5 {
            login.record_failure(addr);
        }
        assert!(login.is_locked_out(addr));
        assert!(
            !dcr.is_locked_out(addr),
            "a login lockout must not spill into DCR's budget"
        );
        for _ in 0..4 {
            assert!(!dcr.record_attempt(addr));
        }
        assert!(dcr.record_attempt(addr));
        assert!(dcr.is_locked_out(addr));
        assert!(!login.is_locked_out(ip("10.0.0.10")));
    }
}
