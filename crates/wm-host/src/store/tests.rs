//! Storage test suite. Each test takes a fresh `Bucket` provided by the
//! caller, so the same suite runs against every backend (in-memory now, and
//! Valkey via testcontainers in tier-3).
//!
//! The case functions are `pub` so an integration test (e.g.,
//! `tests/valkey_storage.rs`) can call them with a Valkey-backed bucket.
//!
//! All Bucket ops can fail at the Valkey backend (network, RESP errors), so
//! every op is fallible and tests `.unwrap()` — a Valkey backend failure
//! during a tier-3 run will panic with a descriptive message.

use super::{Bucket, StoreError};

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

// -- The shared cases ---------------------------------------------------------

pub fn case_get_missing_returns_none(bk: &mut Bucket) {
    assert_eq!(bk.get("absent").unwrap(), None);
}

pub fn case_set_then_get_round_trips(bk: &mut Bucket) {
    bk.set("k", b("v")).unwrap();
    assert_eq!(bk.get("k").unwrap(), Some(b("v")));
}

pub fn case_set_overwrites(bk: &mut Bucket) {
    bk.set("k", b("a")).unwrap();
    bk.set("k", b("b")).unwrap();
    assert_eq!(bk.get("k").unwrap(), Some(b("b")));
}

pub fn case_delete_is_noop_when_absent(bk: &mut Bucket) {
    bk.delete("absent").unwrap();
}

pub fn case_delete_removes_existing(bk: &mut Bucket) {
    bk.set("k", b("v")).unwrap();
    bk.delete("k").unwrap();
    assert_eq!(bk.get("k").unwrap(), None);
}

pub fn case_incr_initializes_missing_key_to_by_value(bk: &mut Bucket) {
    assert_eq!(bk.incr("counter", 5).unwrap(), 5);
    assert_eq!(bk.get("counter").unwrap(), Some(b("5")));
}

pub fn case_incr_increments_existing_integer(bk: &mut Bucket) {
    bk.set("c", b("10")).unwrap();
    assert_eq!(bk.incr("c", 3).unwrap(), 13);
    assert_eq!(bk.incr("c", -5).unwrap(), 8);
}

pub fn case_incr_traps_on_non_integer_value(bk: &mut Bucket) {
    bk.set("c", b("not-a-number")).unwrap();
    let err = bk.incr("c", 1).unwrap_err();
    assert!(matches!(err, StoreError::NotInteger { .. }));
}

/// In-memory only: Valkey collapses overflow and non-integer into the same
/// "ERR value is not an integer or out of range" response, which we report
/// as `NotInteger`. The WIT contract treats both as a trap.
pub fn case_incr_in_memory_overflow(bk: &mut Bucket) {
    bk.set("c", i64::MAX.to_string().into_bytes()).unwrap();
    let err = bk.incr("c", 1).unwrap_err();
    assert!(matches!(err, StoreError::IncrOverflow { .. }));
}

pub fn case_incr_wrong_type_against_list(bk: &mut Bucket) {
    bk.list_push("x", b("v")).unwrap();
    let err = bk.incr("x", 1).unwrap_err();
    assert!(matches!(err, StoreError::WrongType { .. }));
}

pub fn case_list_keys_no_prefix_returns_all(bk: &mut Bucket) {
    bk.set("a", b("1")).unwrap();
    bk.set("b", b("2")).unwrap();
    let mut keys = bk.list_keys(None).unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
}

pub fn case_list_keys_with_prefix_filters(bk: &mut Bucket) {
    bk.set("user:1", b("a")).unwrap();
    bk.set("user:2", b("b")).unwrap();
    bk.set("session:1", b("c")).unwrap();
    let mut keys = bk.list_keys(Some("user:")).unwrap();
    keys.sort();
    assert_eq!(keys, vec!["user:1".to_string(), "user:2".to_string()]);
}

pub fn case_list_push_pop_fifo_order(bk: &mut Bucket) {
    bk.list_push("q", b("a")).unwrap();
    bk.list_push("q", b("b")).unwrap();
    bk.list_push("q", b("c")).unwrap();
    assert_eq!(bk.list_pop("q").unwrap(), Some(b("a")));
    assert_eq!(bk.list_pop("q").unwrap(), Some(b("b")));
    assert_eq!(bk.list_pop("q").unwrap(), Some(b("c")));
    assert_eq!(bk.list_pop("q").unwrap(), None);
}

pub fn case_list_pop_missing_key_is_none(bk: &mut Bucket) {
    assert_eq!(bk.list_pop("absent").unwrap(), None);
}

pub fn case_list_length_zero_for_missing(bk: &mut Bucket) {
    assert_eq!(bk.list_length("absent").unwrap(), 0);
}

pub fn case_list_length_counts_pushes(bk: &mut Bucket) {
    bk.list_push("q", b("a")).unwrap();
    bk.list_push("q", b("b")).unwrap();
    assert_eq!(bk.list_length("q").unwrap(), 2);
}

pub fn case_list_range_positive_indices(bk: &mut Bucket) {
    for v in ["a", "b", "c", "d"] {
        bk.list_push("q", b(v)).unwrap();
    }
    assert_eq!(bk.list_range("q", 0, 1).unwrap(), vec![b("a"), b("b")]);
    assert_eq!(bk.list_range("q", 1, 2).unwrap(), vec![b("b"), b("c")]);
}

pub fn case_list_range_negative_indices_count_from_end(bk: &mut Bucket) {
    for v in ["a", "b", "c", "d"] {
        bk.list_push("q", b(v)).unwrap();
    }
    assert_eq!(bk.list_range("q", -2, -1).unwrap(), vec![b("c"), b("d")]);
    assert_eq!(
        bk.list_range("q", 0, -1).unwrap(),
        vec![b("a"), b("b"), b("c"), b("d")]
    );
}

pub fn case_list_range_empty_when_start_after_stop(bk: &mut Bucket) {
    bk.list_push("q", b("a")).unwrap();
    bk.list_push("q", b("b")).unwrap();
    let empty: Vec<Vec<u8>> = vec![];
    assert_eq!(bk.list_range("q", 1, 0).unwrap(), empty);
}

pub fn case_list_range_missing_key_is_empty(bk: &mut Bucket) {
    let empty: Vec<Vec<u8>> = vec![];
    assert_eq!(bk.list_range("absent", 0, 10).unwrap(), empty);
}

pub fn case_list_push_wrong_type_traps(bk: &mut Bucket) {
    bk.set("k", b("v")).unwrap();
    let err = bk.list_push("k", b("x")).unwrap_err();
    assert!(matches!(err, StoreError::WrongType { .. }));
}

pub fn case_hash_set_then_get(bk: &mut Bucket) {
    bk.hash_set("user:1", "name", b("alice")).unwrap();
    assert_eq!(bk.hash_get("user:1", "name").unwrap(), Some(b("alice")));
}

pub fn case_hash_get_missing_field_is_none(bk: &mut Bucket) {
    bk.hash_set("u", "a", b("1")).unwrap();
    assert_eq!(bk.hash_get("u", "missing").unwrap(), None);
}

pub fn case_hash_get_missing_key_is_none(bk: &mut Bucket) {
    assert_eq!(bk.hash_get("absent", "f").unwrap(), None);
}

pub fn case_hash_delete_removes_field(bk: &mut Bucket) {
    bk.hash_set("u", "a", b("1")).unwrap();
    bk.hash_delete("u", "a").unwrap();
    assert_eq!(bk.hash_get("u", "a").unwrap(), None);
}

pub fn case_hash_keys_returns_field_names(bk: &mut Bucket) {
    bk.hash_set("u", "a", b("1")).unwrap();
    bk.hash_set("u", "b", b("2")).unwrap();
    let mut keys = bk.hash_keys("u").unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
}

pub fn case_hash_keys_missing_is_empty(bk: &mut Bucket) {
    let empty: Vec<String> = vec![];
    assert_eq!(bk.hash_keys("absent").unwrap(), empty);
}

pub fn case_hash_set_wrong_type_traps(bk: &mut Bucket) {
    bk.set("k", b("v")).unwrap();
    let err = bk.hash_set("k", "f", b("v")).unwrap_err();
    assert!(matches!(err, StoreError::WrongType { .. }));
}

pub fn case_set_add_and_contains(bk: &mut Bucket) {
    bk.set_add("seen", "alice").unwrap();
    assert!(bk.set_contains("seen", "alice").unwrap());
    assert!(!bk.set_contains("seen", "bob").unwrap());
}

pub fn case_set_add_idempotent(bk: &mut Bucket) {
    bk.set_add("seen", "alice").unwrap();
    bk.set_add("seen", "alice").unwrap();
    assert!(bk.set_contains("seen", "alice").unwrap());
}

pub fn case_set_remove_existing(bk: &mut Bucket) {
    bk.set_add("seen", "alice").unwrap();
    bk.set_remove("seen", "alice").unwrap();
    assert!(!bk.set_contains("seen", "alice").unwrap());
}

pub fn case_set_remove_missing_is_noop(bk: &mut Bucket) {
    bk.set_remove("absent", "x").unwrap();
}

pub fn case_set_contains_missing_key_is_false(bk: &mut Bucket) {
    assert!(!bk.set_contains("absent", "x").unwrap());
}

pub fn case_set_add_wrong_type_traps(bk: &mut Bucket) {
    bk.set("k", b("v")).unwrap();
    let err = bk.set_add("k", "m").unwrap_err();
    assert!(matches!(err, StoreError::WrongType { .. }));
}

/// Both backends must agree that a second claim on the same key
/// fails. This is what makes the user-email index safe against two
/// replicas cold-starting at once (ADR-0037 item 6).
pub fn case_set_if_absent_claims_once(bk: &mut Bucket) {
    assert!(bk.set_if_absent("claim", b"first".to_vec()).unwrap());
    assert!(!bk.set_if_absent("claim", b"second".to_vec()).unwrap());
    assert_eq!(bk.get("claim").unwrap().as_deref(), Some(&b"first"[..]));
}

/// A lease is exclusive while held. The expiry half can't be tested
/// without sleeping past it, so this covers the property that matters
/// per tick: a second caller is refused.
pub fn case_lease_is_exclusive_while_held(bk: &mut Bucket) {
    assert!(bk.try_acquire_lease("lease", 60).unwrap());
    assert!(!bk.try_acquire_lease("lease", 60).unwrap());
}

/// Cases shared between in-memory and Valkey runs. Backend-specific cases
/// (e.g., `case_incr_in_memory_overflow`) are listed separately.
#[macro_export]
macro_rules! storage_cases {
    ($macro:ident) => {
        $macro!(
            case_set_if_absent_claims_once,
            case_lease_is_exclusive_while_held,
            case_get_missing_returns_none,
            case_set_then_get_round_trips,
            case_set_overwrites,
            case_delete_is_noop_when_absent,
            case_delete_removes_existing,
            case_incr_initializes_missing_key_to_by_value,
            case_incr_increments_existing_integer,
            case_incr_traps_on_non_integer_value,
            case_incr_wrong_type_against_list,
            case_list_keys_no_prefix_returns_all,
            case_list_keys_with_prefix_filters,
            case_list_push_pop_fifo_order,
            case_list_pop_missing_key_is_none,
            case_list_length_zero_for_missing,
            case_list_length_counts_pushes,
            case_list_range_positive_indices,
            case_list_range_negative_indices_count_from_end,
            case_list_range_empty_when_start_after_stop,
            case_list_range_missing_key_is_empty,
            case_list_push_wrong_type_traps,
            case_hash_set_then_get,
            case_hash_get_missing_field_is_none,
            case_hash_get_missing_key_is_none,
            case_hash_delete_removes_field,
            case_hash_keys_returns_field_names,
            case_hash_keys_missing_is_empty,
            case_hash_set_wrong_type_traps,
            case_set_add_and_contains,
            case_set_add_idempotent,
            case_set_remove_existing,
            case_set_remove_missing_is_noop,
            case_set_contains_missing_key_is_false,
            case_set_add_wrong_type_traps,
        );
    };
}

// -- In-memory specifics ------------------------------------------------------
// Tier-1 entry points: each test gets a fresh in-memory bucket and calls one
// case. Keeping them as `#[test]` functions (rather than a single
// parameterized harness) means failures point at a specific case.

#[cfg(test)]
mod in_memory {
    use super::*;
    use crate::store::Storage;

    fn fresh() -> Bucket {
        Storage::in_memory().route_bucket("g", "r").unwrap()
    }

    /// An expired lease must be reclaimable.
    ///
    /// In-memory only, deliberately. Valkey enforces expiry itself
    /// through `EX`, which is Valkey's guarantee to keep; what needs
    /// testing is *our* deadline comparison, which exists only on this
    /// backend and only because `set_ttl` is a no-op here. Without it a
    /// lease would be taken once and never released — wedging the
    /// lifecycle sweeper after a single pass and locking out any IP
    /// that ever tripped the login throttle. Writing a past deadline
    /// directly beats sleeping a real second in the test suite.
    #[test]
    fn expired_lease_is_reclaimable() {
        let mut bk = fresh();
        assert!(bk.try_acquire_lease("lease", 60).unwrap());
        assert!(!bk.try_acquire_lease("lease", 60).unwrap(), "held");

        let past = (chrono::Utc::now() - chrono::Duration::seconds(1))
            .to_rfc3339()
            .into_bytes();
        bk.set("lease", past).unwrap();

        assert!(
            bk.try_acquire_lease("lease", 60).unwrap(),
            "a lease whose deadline has passed can be taken again"
        );
    }

    /// A lease value we can't parse is treated as expired rather than
    /// held forever — otherwise one corrupt key would wedge the sweeper
    /// permanently, with no way back short of manual intervention.
    #[test]
    fn unparseable_lease_is_treated_as_expired() {
        let mut bk = fresh();
        bk.set("lease", b"not-a-timestamp".to_vec()).unwrap();
        assert!(bk.try_acquire_lease("lease", 60).unwrap());
    }

    macro_rules! decl_cases {
        ($($name:ident),* $(,)?) => {
            $(
                #[test]
                fn $name() {
                    let mut bk = fresh();
                    super::$name(&mut bk);
                }
            )*
        };
    }

    crate::storage_cases!(decl_cases);

    // Backend-specific case: in-memory has explicit overflow detection.
    #[test]
    fn case_incr_in_memory_overflow() {
        let mut bk = fresh();
        super::case_incr_in_memory_overflow(&mut bk);
    }

    #[test]
    fn buckets_with_different_prefixes_are_isolated() {
        let storage = Storage::in_memory();
        let mut a = storage.route_bucket("group-a", "route-1").unwrap();
        let mut b = storage.route_bucket("group-b", "route-1").unwrap();
        a.set("shared-key", super::b("from-a")).unwrap();
        b.set("shared-key", super::b("from-b")).unwrap();
        assert_eq!(a.get("shared-key").unwrap(), Some(super::b("from-a")));
        assert_eq!(b.get("shared-key").unwrap(), Some(super::b("from-b")));
    }

    #[test]
    fn route_and_group_buckets_are_isolated_within_same_group() {
        let storage = Storage::in_memory();
        let mut route = storage.route_bucket("g", "r").unwrap();
        let mut group = storage.group_bucket("g").unwrap();
        route.set("k", super::b("route")).unwrap();
        group.set("k", super::b("group")).unwrap();
        assert_eq!(route.get("k").unwrap(), Some(super::b("route")));
        assert_eq!(group.get("k").unwrap(), Some(super::b("group")));
    }

    #[test]
    fn list_keys_only_returns_this_buckets_keys() {
        let storage = Storage::in_memory();
        let mut a = storage.route_bucket("g", "r1").unwrap();
        let mut b = storage.route_bucket("g", "r2").unwrap();
        a.set("alice", super::b("1")).unwrap();
        b.set("bob", super::b("2")).unwrap();
        let keys_a = a.list_keys(None).unwrap();
        assert_eq!(keys_a, vec!["alice".to_string()]);
    }
}
