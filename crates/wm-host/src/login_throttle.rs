//! Per-IP login throttle for `POST /auth/login/password`.
//!
//! ADR-0018 calls for "5 failed attempts in 60 seconds triggers a
//! 60-second lockout, in-memory counter". This isn't sufficient to
//! make local auth safe to expose publicly — that's not the use case —
//! but it stops trivial drive-by brute force inside the trusted-
//! network threat model.
//!
//! Implementation: a `Mutex<HashMap<IpAddr, Counter>>`. Each `Counter`
//! tracks the timestamp of the most recent failed attempt and a
//! sliding count. A successful login clears the counter for that IP.
//! Stale entries are reaped opportunistically when the IP is seen
//! again — there's no background sweeper, which means the map can
//! grow within the lockout window. For the threat model that's fine.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct Counter {
    /// Failed attempts within the active window. Reset to 0 when the
    /// window elapses or on a successful login.
    fails: u32,
    /// When the current window started. The window slides forward on
    /// every miss; we don't reset it on a hit because the hit clears
    /// the counter entirely.
    window_started: Instant,
    /// `Some(until)` when locked out; comparison vs `Instant::now()`
    /// decides whether to still reject.
    locked_until: Option<Instant>,
}

#[derive(Default)]
pub struct LoginThrottle {
    counters: Mutex<HashMap<IpAddr, Counter>>,
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether `ip` is currently locked out. Doesn't mutate any
    /// state — the caller checks before attempting verification.
    pub fn is_locked_out(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let map = self.counters.lock().expect("login throttle mutex");
        map.get(&ip)
            .and_then(|c| c.locked_until)
            .is_some_and(|until| until > now)
    }

    /// Record a failed login attempt for `ip`. Returns `true` if the
    /// failure pushed the IP into lockout (the caller may want to
    /// log the transition).
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.counters.lock().expect("login throttle mutex");
        let counter = map.entry(ip).or_insert(Counter {
            fails: 0,
            window_started: now,
            locked_until: None,
        });
        // If we're past an existing lockout, clear it before counting
        // this failure — otherwise the IP would stay locked forever.
        if let Some(until) = counter.locked_until
            && until <= now
        {
            counter.locked_until = None;
            counter.fails = 0;
            counter.window_started = now;
        }
        // Slide the window: if the existing window is stale, start
        // a fresh one with this failure.
        if now.duration_since(counter.window_started) > WINDOW {
            counter.fails = 0;
            counter.window_started = now;
        }
        counter.fails += 1;
        if counter.fails >= MAX_FAILS {
            counter.locked_until = Some(now + LOCKOUT);
            true
        } else {
            false
        }
    }

    /// Clear the counter for `ip` after a successful login.
    pub fn record_success(&self, ip: IpAddr) {
        let mut map = self.counters.lock().expect("login throttle mutex");
        map.remove(&ip);
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

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn fresh_ip_is_not_locked() {
        let t = LoginThrottle::new();
        assert!(!t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn four_failures_dont_lock_out() {
        let t = LoginThrottle::new();
        for _ in 0..4 {
            assert!(!t.record_failure(ip("10.0.0.1")));
        }
        assert!(!t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn fifth_failure_triggers_lockout() {
        let t = LoginThrottle::new();
        for _ in 0..4 {
            t.record_failure(ip("10.0.0.1"));
        }
        assert!(t.record_failure(ip("10.0.0.1")));
        assert!(t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn success_clears_the_counter() {
        let t = LoginThrottle::new();
        for _ in 0..3 {
            t.record_failure(ip("10.0.0.1"));
        }
        t.record_success(ip("10.0.0.1"));
        // Counter cleared → can fail another full round without lock.
        for _ in 0..4 {
            assert!(!t.record_failure(ip("10.0.0.1")));
        }
    }

    #[test]
    fn lockout_is_per_ip() {
        let t = LoginThrottle::new();
        for _ in 0..5 {
            t.record_failure(ip("10.0.0.1"));
        }
        assert!(t.is_locked_out(ip("10.0.0.1")));
        // A different IP starts fresh — the lockout doesn't leak.
        assert!(!t.is_locked_out(ip("10.0.0.2")));
    }

    #[test]
    fn time_window_slide_resets_counter() {
        // We can't easily fast-forward `Instant`, so instead exercise
        // the `window_started` reset by manipulating the internals.
        // This is a bit invasive but the alternative is a 60s test.
        let t = LoginThrottle::new();
        for _ in 0..4 {
            t.record_failure(ip("10.0.0.1"));
        }
        {
            let mut map = t.counters.lock().unwrap();
            let counter = map.get_mut(&ip("10.0.0.1")).unwrap();
            // Pretend the window started 2 minutes ago.
            counter.window_started = Instant::now() - Duration::from_secs(120);
        }
        // Next failure starts a new window; we shouldn't be locked
        // out (1 fail in the fresh window).
        assert!(!t.record_failure(ip("10.0.0.1")));
        assert!(!t.is_locked_out(ip("10.0.0.1")));
    }

    #[test]
    fn lockout_clears_after_window() {
        let t = LoginThrottle::new();
        for _ in 0..5 {
            t.record_failure(ip("10.0.0.1"));
        }
        assert!(t.is_locked_out(ip("10.0.0.1")));
        // Fast-forward the lockout into the past.
        {
            let mut map = t.counters.lock().unwrap();
            let counter = map.get_mut(&ip("10.0.0.1")).unwrap();
            counter.locked_until = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(!t.is_locked_out(ip("10.0.0.1")));
        // A subsequent failure starts a fresh window.
        assert!(!t.record_failure(ip("10.0.0.1")));
    }
}
