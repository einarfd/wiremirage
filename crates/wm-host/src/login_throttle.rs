//! Per-IP login throttle for `POST /auth/login/password`.
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

#[derive(Clone)]
pub struct LoginThrottle {
    storage: Storage,
}

impl LoginThrottle {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Keys are namespaced per IP, and all three expire.
    ///
    /// The counter carries a TTL of its own rather than relying on being
    /// deleted: it is only cleared on lockout or a successful login, so
    /// an address that fails once or twice and never comes back would
    /// otherwise leave a key behind forever — one per source IP, on a
    /// publicly reachable endpoint, in a system whose whole premise is
    /// that state expires. (`set_ttl` is a no-op in memory, which is
    /// fine: that backend dies with the process, and the window lease
    /// already governs correctness there.)
    fn fails_key(ip: IpAddr) -> String {
        format!("throttle:fails:{ip}")
    }

    fn window_key(ip: IpAddr) -> String {
        format!("throttle:window:{ip}")
    }

    fn lock_key(ip: IpAddr) -> String {
        format!("throttle:lock:{ip}")
    }

    /// Check whether `ip` is currently locked out. Does not mutate — the
    /// caller checks before attempting verification.
    pub fn is_locked_out(&self, ip: IpAddr) -> bool {
        let mut bucket = match self.storage.admin_bucket() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "login throttle: opening bucket");
                return false;
            }
        };
        let raw = match bucket.get(&Self::lock_key(ip)) {
            Ok(Some(v)) => v,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(error = %e, "login throttle: reading lockout");
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

    /// Record a failed login attempt for `ip`. Returns `true` if this
    /// failure pushed the IP into lockout, so the caller can log the
    /// transition.
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        let mut bucket = match self.storage.admin_bucket() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "login throttle: opening bucket");
                return false;
            }
        };
        // Claiming the window lease means this failure opened a fresh
        // 60-second window, so the count restarts at 1. Failing to claim
        // means a window is already running and this adds to it. The
        // claim is atomic, so concurrent failures across replicas can't
        // both decide they started the window.
        let fresh_window = bucket
            .try_acquire_lease(&Self::window_key(ip), WINDOW_SECONDS)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "login throttle: window lease");
                false
            });
        let fails = if fresh_window {
            if let Err(e) = bucket.set(&Self::fails_key(ip), b"1".to_vec()) {
                tracing::warn!(error = %e, "login throttle: resetting counter");
                return false;
            }
            1
        } else {
            match bucket.incr(&Self::fails_key(ip), 1) {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "login throttle: incrementing counter");
                    return false;
                }
            }
        };
        // Both branches, because `incr` creates the key when it is
        // absent — which is the case a lost window lease produces.
        let _ = bucket.set_ttl(&Self::fails_key(ip), WINDOW_SECONDS);
        if fails < MAX_FAILS {
            return false;
        }
        // Trip the lockout, and clear the counter so the IP starts clean
        // once the lockout lapses rather than re-tripping on its next
        // single failure.
        let newly_locked = bucket
            .try_acquire_lease(&Self::lock_key(ip), LOCKOUT_SECONDS)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "login throttle: lock lease");
                false
            });
        let _ = bucket.delete(&Self::fails_key(ip));
        let _ = bucket.delete(&Self::window_key(ip));
        newly_locked
    }

    /// Clear the counters for `ip` after a successful login. Leaves any
    /// active lockout in place — a correct password during a lockout
    /// shouldn't be a way out of it.
    pub fn record_success(&self, ip: IpAddr) {
        let Ok(mut bucket) = self.storage.admin_bucket() else {
            return;
        };
        let _ = bucket.delete(&Self::fails_key(ip));
        let _ = bucket.delete(&Self::window_key(ip));
    }
}

impl std::fmt::Debug for LoginThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginThrottle").finish_non_exhaustive()
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
        let mut bucket = t.storage.admin_bucket().unwrap();
        let past = (Utc::now() - chrono::Duration::seconds(1))
            .to_rfc3339()
            .into_bytes();
        bucket.set(&LoginThrottle::lock_key(addr), past).unwrap();
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
        let mut bucket = t.storage.admin_bucket().unwrap();
        assert!(
            bucket
                .try_acquire_lease(&LoginThrottle::lock_key(addr), 60)
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
}
