use std::collections::{HashMap, HashSet, VecDeque};

use thiserror::Error;

/// In-memory backing for a single `store.bucket`. Acts as the slice 1 stand-in
/// for the per-route / per-group Valkey-backed store that arrives in slice 2.
///
/// Modelled after Valkey's per-key type discipline: a key has exactly one type
/// (bytes, list, hash, or set) and using the wrong-type op against it is a
/// reportable error rather than a silent type promotion.
#[derive(Debug, Default)]
pub struct MemBucket {
    data: HashMap<String, MemValue>,
}

#[derive(Debug)]
enum MemValue {
    Bytes(Vec<u8>),
    List(VecDeque<Vec<u8>>),
    Hash(HashMap<String, Vec<u8>>),
    Set(HashSet<String>),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("WRONGTYPE: key {key:?} holds {actual}, op requires {expected}")]
    WrongType {
        key: String,
        actual: &'static str,
        expected: &'static str,
    },
    #[error("VALUE: key {key:?} value is not a valid signed integer")]
    NotInteger { key: String },
    #[error("OVERFLOW: incr on key {key:?} would overflow s64")]
    IncrOverflow { key: String },
}

impl MemValue {
    fn kind(&self) -> &'static str {
        match self {
            MemValue::Bytes(_) => "bytes",
            MemValue::List(_) => "list",
            MemValue::Hash(_) => "hash",
            MemValue::Set(_) => "set",
        }
    }
}

impl MemBucket {
    pub fn new() -> Self {
        Self::default()
    }

    // -- Basic key-value operations --

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self.data.get(key) {
            None => Ok(None),
            Some(MemValue::Bytes(b)) => Ok(Some(b.clone())),
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "bytes",
            }),
        }
    }

    pub fn set(&mut self, key: String, value: Vec<u8>) {
        self.data.insert(key, MemValue::Bytes(value));
    }

    pub fn delete(&mut self, key: &str) {
        self.data.remove(key);
    }

    pub fn incr(&mut self, key: &str, by: i64) -> Result<i64, StoreError> {
        match self.data.get_mut(key) {
            None => {
                self.data.insert(
                    key.to_string(),
                    MemValue::Bytes(by.to_string().into_bytes()),
                );
                Ok(by)
            }
            Some(MemValue::Bytes(b)) => {
                let s = std::str::from_utf8(b).map_err(|_| StoreError::NotInteger {
                    key: key.to_string(),
                })?;
                let cur: i64 = s.parse().map_err(|_| StoreError::NotInteger {
                    key: key.to_string(),
                })?;
                let next = cur.checked_add(by).ok_or(StoreError::IncrOverflow {
                    key: key.to_string(),
                })?;
                *b = next.to_string().into_bytes();
                Ok(next)
            }
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "bytes",
            }),
        }
    }

    pub fn list_keys(&self, prefix: Option<&str>) -> Vec<String> {
        match prefix {
            None => self.data.keys().cloned().collect(),
            Some(p) => self
                .data
                .keys()
                .filter(|k| k.starts_with(p))
                .cloned()
                .collect(),
        }
    }

    // -- List operations --

    pub fn list_push(&mut self, key: String, value: Vec<u8>) -> Result<(), StoreError> {
        match self.data.entry(key.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut q = VecDeque::new();
                q.push_back(value);
                e.insert(MemValue::List(q));
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                MemValue::List(q) => {
                    q.push_back(value);
                    Ok(())
                }
                other => Err(StoreError::WrongType {
                    key,
                    actual: other.kind(),
                    expected: "list",
                }),
            },
        }
    }

    pub fn list_pop(&mut self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self.data.get_mut(key) {
            None => Ok(None),
            Some(MemValue::List(q)) => Ok(q.pop_front()),
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "list",
            }),
        }
    }

    pub fn list_range(&self, key: &str, start: i64, stop: i64) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.data.get(key) {
            None => Ok(Vec::new()),
            Some(MemValue::List(q)) => {
                let len = q.len() as i64;
                if len == 0 {
                    return Ok(Vec::new());
                }
                let normalize = |i: i64| -> i64 {
                    if i < 0 {
                        (len + i).max(0)
                    } else {
                        i.min(len - 1)
                    }
                };
                let s = normalize(start);
                let e = normalize(stop);
                if s > e {
                    return Ok(Vec::new());
                }
                Ok(q.iter()
                    .skip(s as usize)
                    .take((e - s + 1) as usize)
                    .cloned()
                    .collect())
            }
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "list",
            }),
        }
    }

    pub fn list_length(&self, key: &str) -> Result<u64, StoreError> {
        match self.data.get(key) {
            None => Ok(0),
            Some(MemValue::List(q)) => Ok(q.len() as u64),
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "list",
            }),
        }
    }

    // -- Hash operations --

    pub fn hash_get(&self, key: &str, field: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self.data.get(key) {
            None => Ok(None),
            Some(MemValue::Hash(h)) => Ok(h.get(field).cloned()),
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "hash",
            }),
        }
    }

    pub fn hash_set(
        &mut self,
        key: String,
        field: String,
        value: Vec<u8>,
    ) -> Result<(), StoreError> {
        match self.data.entry(key.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut h = HashMap::new();
                h.insert(field, value);
                e.insert(MemValue::Hash(h));
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                MemValue::Hash(h) => {
                    h.insert(field, value);
                    Ok(())
                }
                other => Err(StoreError::WrongType {
                    key,
                    actual: other.kind(),
                    expected: "hash",
                }),
            },
        }
    }

    pub fn hash_delete(&mut self, key: &str, field: &str) -> Result<(), StoreError> {
        match self.data.get_mut(key) {
            None => Ok(()),
            Some(MemValue::Hash(h)) => {
                h.remove(field);
                Ok(())
            }
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "hash",
            }),
        }
    }

    pub fn hash_keys(&self, key: &str) -> Result<Vec<String>, StoreError> {
        match self.data.get(key) {
            None => Ok(Vec::new()),
            Some(MemValue::Hash(h)) => Ok(h.keys().cloned().collect()),
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "hash",
            }),
        }
    }

    // -- Set operations --

    pub fn set_add(&mut self, key: String, member: String) -> Result<(), StoreError> {
        match self.data.entry(key.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut s = HashSet::new();
                s.insert(member);
                e.insert(MemValue::Set(s));
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                MemValue::Set(s) => {
                    s.insert(member);
                    Ok(())
                }
                other => Err(StoreError::WrongType {
                    key,
                    actual: other.kind(),
                    expected: "set",
                }),
            },
        }
    }

    pub fn set_remove(&mut self, key: &str, member: &str) -> Result<(), StoreError> {
        match self.data.get_mut(key) {
            None => Ok(()),
            Some(MemValue::Set(s)) => {
                s.remove(member);
                Ok(())
            }
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "set",
            }),
        }
    }

    pub fn set_contains(&self, key: &str, member: &str) -> Result<bool, StoreError> {
        match self.data.get(key) {
            None => Ok(false),
            Some(MemValue::Set(s)) => Ok(s.contains(member)),
            Some(other) => Err(StoreError::WrongType {
                key: key.to_string(),
                actual: other.kind(),
                expected: "set",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    // -- Basic KV --

    #[test]
    fn get_missing_returns_none() {
        let bk = MemBucket::new();
        assert_eq!(bk.get("absent").unwrap(), None);
    }

    #[test]
    fn set_then_get_round_trips() {
        let mut bk = MemBucket::new();
        bk.set("k".into(), b("v"));
        assert_eq!(bk.get("k").unwrap(), Some(b("v")));
    }

    #[test]
    fn set_overwrites() {
        let mut bk = MemBucket::new();
        bk.set("k".into(), b("a"));
        bk.set("k".into(), b("b"));
        assert_eq!(bk.get("k").unwrap(), Some(b("b")));
    }

    #[test]
    fn delete_is_noop_when_absent() {
        let mut bk = MemBucket::new();
        bk.delete("absent");
    }

    #[test]
    fn delete_removes_existing() {
        let mut bk = MemBucket::new();
        bk.set("k".into(), b("v"));
        bk.delete("k");
        assert_eq!(bk.get("k").unwrap(), None);
    }

    // -- incr --

    #[test]
    fn incr_initializes_missing_key_to_by_value() {
        let mut bk = MemBucket::new();
        assert_eq!(bk.incr("counter", 5).unwrap(), 5);
        assert_eq!(bk.get("counter").unwrap(), Some(b("5")));
    }

    #[test]
    fn incr_increments_existing_integer() {
        let mut bk = MemBucket::new();
        bk.set("c".into(), b("10"));
        assert_eq!(bk.incr("c", 3).unwrap(), 13);
        assert_eq!(bk.incr("c", -5).unwrap(), 8);
    }

    #[test]
    fn incr_traps_on_non_integer_value() {
        let mut bk = MemBucket::new();
        bk.set("c".into(), b("not-a-number"));
        let err = bk.incr("c", 1).unwrap_err();
        assert!(matches!(err, StoreError::NotInteger { .. }));
    }

    #[test]
    fn incr_traps_on_overflow() {
        let mut bk = MemBucket::new();
        bk.set("c".into(), i64::MAX.to_string().into_bytes());
        let err = bk.incr("c", 1).unwrap_err();
        assert!(matches!(err, StoreError::IncrOverflow { .. }));
    }

    #[test]
    fn incr_wrong_type_against_list() {
        let mut bk = MemBucket::new();
        bk.list_push("x".into(), b("v")).unwrap();
        let err = bk.incr("x", 1).unwrap_err();
        assert!(matches!(err, StoreError::WrongType { .. }));
    }

    // -- list_keys --

    #[test]
    fn list_keys_no_prefix_returns_all() {
        let mut bk = MemBucket::new();
        bk.set("a".into(), b("1"));
        bk.set("b".into(), b("2"));
        let mut keys = bk.list_keys(None);
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn list_keys_with_prefix_filters() {
        let mut bk = MemBucket::new();
        bk.set("user:1".into(), b("a"));
        bk.set("user:2".into(), b("b"));
        bk.set("session:1".into(), b("c"));
        let mut keys = bk.list_keys(Some("user:"));
        keys.sort();
        assert_eq!(keys, vec!["user:1".to_string(), "user:2".to_string()]);
    }

    // -- List ops --

    #[test]
    fn list_push_pop_fifo_order() {
        let mut bk = MemBucket::new();
        bk.list_push("q".into(), b("a")).unwrap();
        bk.list_push("q".into(), b("b")).unwrap();
        bk.list_push("q".into(), b("c")).unwrap();
        assert_eq!(bk.list_pop("q").unwrap(), Some(b("a")));
        assert_eq!(bk.list_pop("q").unwrap(), Some(b("b")));
        assert_eq!(bk.list_pop("q").unwrap(), Some(b("c")));
        assert_eq!(bk.list_pop("q").unwrap(), None);
    }

    #[test]
    fn list_pop_missing_key_is_none() {
        let mut bk = MemBucket::new();
        assert_eq!(bk.list_pop("absent").unwrap(), None);
    }

    #[test]
    fn list_length_zero_for_missing() {
        let bk = MemBucket::new();
        assert_eq!(bk.list_length("absent").unwrap(), 0);
    }

    #[test]
    fn list_length_counts_pushes() {
        let mut bk = MemBucket::new();
        bk.list_push("q".into(), b("a")).unwrap();
        bk.list_push("q".into(), b("b")).unwrap();
        assert_eq!(bk.list_length("q").unwrap(), 2);
    }

    #[test]
    fn list_range_positive_indices() {
        let mut bk = MemBucket::new();
        for v in ["a", "b", "c", "d"] {
            bk.list_push("q".into(), b(v)).unwrap();
        }
        assert_eq!(bk.list_range("q", 0, 1).unwrap(), vec![b("a"), b("b")]);
        assert_eq!(bk.list_range("q", 1, 2).unwrap(), vec![b("b"), b("c")]);
    }

    #[test]
    fn list_range_negative_indices_count_from_end() {
        let mut bk = MemBucket::new();
        for v in ["a", "b", "c", "d"] {
            bk.list_push("q".into(), b(v)).unwrap();
        }
        assert_eq!(bk.list_range("q", -2, -1).unwrap(), vec![b("c"), b("d")]);
        assert_eq!(
            bk.list_range("q", 0, -1).unwrap(),
            vec![b("a"), b("b"), b("c"), b("d")]
        );
    }

    #[test]
    fn list_range_empty_when_start_after_stop() {
        let mut bk = MemBucket::new();
        bk.list_push("q".into(), b("a")).unwrap();
        bk.list_push("q".into(), b("b")).unwrap();
        let empty: Vec<Vec<u8>> = vec![];
        assert_eq!(bk.list_range("q", 1, 0).unwrap(), empty);
    }

    #[test]
    fn list_range_missing_key_is_empty() {
        let bk = MemBucket::new();
        let empty: Vec<Vec<u8>> = vec![];
        assert_eq!(bk.list_range("absent", 0, 10).unwrap(), empty);
    }

    #[test]
    fn list_push_wrong_type_traps() {
        let mut bk = MemBucket::new();
        bk.set("k".into(), b("v"));
        let err = bk.list_push("k".into(), b("x")).unwrap_err();
        assert!(matches!(err, StoreError::WrongType { .. }));
    }

    // -- Hash ops --

    #[test]
    fn hash_set_then_get() {
        let mut bk = MemBucket::new();
        bk.hash_set("user:1".into(), "name".into(), b("alice"))
            .unwrap();
        assert_eq!(bk.hash_get("user:1", "name").unwrap(), Some(b("alice")));
    }

    #[test]
    fn hash_get_missing_field_is_none() {
        let mut bk = MemBucket::new();
        bk.hash_set("u".into(), "a".into(), b("1")).unwrap();
        assert_eq!(bk.hash_get("u", "missing").unwrap(), None);
    }

    #[test]
    fn hash_get_missing_key_is_none() {
        let bk = MemBucket::new();
        assert_eq!(bk.hash_get("absent", "f").unwrap(), None);
    }

    #[test]
    fn hash_delete_removes_field() {
        let mut bk = MemBucket::new();
        bk.hash_set("u".into(), "a".into(), b("1")).unwrap();
        bk.hash_delete("u", "a").unwrap();
        assert_eq!(bk.hash_get("u", "a").unwrap(), None);
    }

    #[test]
    fn hash_keys_returns_field_names() {
        let mut bk = MemBucket::new();
        bk.hash_set("u".into(), "a".into(), b("1")).unwrap();
        bk.hash_set("u".into(), "b".into(), b("2")).unwrap();
        let mut keys = bk.hash_keys("u").unwrap();
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn hash_keys_missing_is_empty() {
        let bk = MemBucket::new();
        let empty: Vec<String> = vec![];
        assert_eq!(bk.hash_keys("absent").unwrap(), empty);
    }

    #[test]
    fn hash_set_wrong_type_traps() {
        let mut bk = MemBucket::new();
        bk.set("k".into(), b("v"));
        let err = bk.hash_set("k".into(), "f".into(), b("v")).unwrap_err();
        assert!(matches!(err, StoreError::WrongType { .. }));
    }

    // -- Set ops --

    #[test]
    fn set_add_and_contains() {
        let mut bk = MemBucket::new();
        bk.set_add("seen".into(), "alice".into()).unwrap();
        assert!(bk.set_contains("seen", "alice").unwrap());
        assert!(!bk.set_contains("seen", "bob").unwrap());
    }

    #[test]
    fn set_add_idempotent() {
        let mut bk = MemBucket::new();
        bk.set_add("seen".into(), "alice".into()).unwrap();
        bk.set_add("seen".into(), "alice".into()).unwrap();
        assert!(bk.set_contains("seen", "alice").unwrap());
    }

    #[test]
    fn set_remove_existing() {
        let mut bk = MemBucket::new();
        bk.set_add("seen".into(), "alice".into()).unwrap();
        bk.set_remove("seen", "alice").unwrap();
        assert!(!bk.set_contains("seen", "alice").unwrap());
    }

    #[test]
    fn set_remove_missing_is_noop() {
        let mut bk = MemBucket::new();
        bk.set_remove("absent", "x").unwrap();
    }

    #[test]
    fn set_contains_missing_key_is_false() {
        let bk = MemBucket::new();
        assert!(!bk.set_contains("absent", "x").unwrap());
    }

    #[test]
    fn set_add_wrong_type_traps() {
        let mut bk = MemBucket::new();
        bk.set("k".into(), b("v"));
        let err = bk.set_add("k".into(), "m".into()).unwrap_err();
        assert!(matches!(err, StoreError::WrongType { .. }));
    }
}
