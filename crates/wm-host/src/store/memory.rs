//! In-memory backing store. Operations here work on a borrowed `MemStore`
//! (a `HashMap<String, MemValue>`) — the locking and prefix logic lives in
//! `Bucket::InMemory` in the parent module.

use std::collections::{HashMap, HashSet, VecDeque};

use super::StoreError;

pub type MemStore = HashMap<String, MemValue>;

#[derive(Debug)]
pub enum MemValue {
    Bytes(Vec<u8>),
    List(VecDeque<Vec<u8>>),
    Hash(HashMap<String, Vec<u8>>),
    Set(HashSet<String>),
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

fn wrong_type(key: &str, actual: &'static str, expected: &'static str) -> StoreError {
    StoreError::WrongType {
        key: key.to_string(),
        actual,
        expected,
    }
}

// -- Basic key-value --

pub fn get(store: &MemStore, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    match store.get(key) {
        None => Ok(None),
        Some(MemValue::Bytes(b)) => Ok(Some(b.clone())),
        Some(other) => Err(wrong_type(key, other.kind(), "bytes")),
    }
}

pub fn set(store: &mut MemStore, key: String, value: Vec<u8>) {
    store.insert(key, MemValue::Bytes(value));
}

pub fn delete(store: &mut MemStore, key: &str) {
    store.remove(key);
}

pub fn incr(store: &mut MemStore, key: &str, by: i64) -> Result<i64, StoreError> {
    match store.get_mut(key) {
        None => {
            store.insert(
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
        Some(other) => Err(wrong_type(key, other.kind(), "bytes")),
    }
}

/// List keys in the store whose `key` starts with `bucket_prefix + user_prefix`.
/// Returns the user-facing key (with `bucket_prefix` stripped). The combined
/// prefix is computed by the caller; this helper just scans.
pub fn scan_with_prefix(store: &MemStore, full_prefix: &str) -> Vec<String> {
    store
        .keys()
        .filter(|k| k.starts_with(full_prefix))
        .cloned()
        .collect()
}

// -- List ops --

pub fn list_push(store: &mut MemStore, key: String, value: Vec<u8>) -> Result<(), StoreError> {
    match store.entry(key.clone()) {
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
            other => Err(wrong_type(&key, other.kind(), "list")),
        },
    }
}

pub fn list_pop(store: &mut MemStore, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    match store.get_mut(key) {
        None => Ok(None),
        Some(MemValue::List(q)) => Ok(q.pop_front()),
        Some(other) => Err(wrong_type(key, other.kind(), "list")),
    }
}

pub fn list_range(
    store: &MemStore,
    key: &str,
    start: i64,
    stop: i64,
) -> Result<Vec<Vec<u8>>, StoreError> {
    match store.get(key) {
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
        Some(other) => Err(wrong_type(key, other.kind(), "list")),
    }
}

pub fn list_length(store: &MemStore, key: &str) -> Result<u64, StoreError> {
    match store.get(key) {
        None => Ok(0),
        Some(MemValue::List(q)) => Ok(q.len() as u64),
        Some(other) => Err(wrong_type(key, other.kind(), "list")),
    }
}

// -- Hash ops --

pub fn hash_get(store: &MemStore, key: &str, field: &str) -> Result<Option<Vec<u8>>, StoreError> {
    match store.get(key) {
        None => Ok(None),
        Some(MemValue::Hash(h)) => Ok(h.get(field).cloned()),
        Some(other) => Err(wrong_type(key, other.kind(), "hash")),
    }
}

pub fn hash_set(
    store: &mut MemStore,
    key: String,
    field: String,
    value: Vec<u8>,
) -> Result<(), StoreError> {
    match store.entry(key.clone()) {
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
            other => Err(wrong_type(&key, other.kind(), "hash")),
        },
    }
}

pub fn hash_delete(store: &mut MemStore, key: &str, field: &str) -> Result<(), StoreError> {
    match store.get_mut(key) {
        None => Ok(()),
        Some(MemValue::Hash(h)) => {
            h.remove(field);
            Ok(())
        }
        Some(other) => Err(wrong_type(key, other.kind(), "hash")),
    }
}

pub fn hash_keys(store: &MemStore, key: &str) -> Result<Vec<String>, StoreError> {
    match store.get(key) {
        None => Ok(Vec::new()),
        Some(MemValue::Hash(h)) => Ok(h.keys().cloned().collect()),
        Some(other) => Err(wrong_type(key, other.kind(), "hash")),
    }
}

/// Atomic increment of a hash field interpreted as a signed integer. Mirrors
/// Valkey's HINCRBY. Initializes to `by` if the key/field is absent.
pub fn hash_incr(store: &mut MemStore, key: &str, field: &str, by: i64) -> Result<i64, StoreError> {
    let entry = store
        .entry(key.to_string())
        .or_insert_with(|| MemValue::Hash(HashMap::new()));
    match entry {
        MemValue::Hash(h) => match h.get(field) {
            None => {
                h.insert(field.to_string(), by.to_string().into_bytes());
                Ok(by)
            }
            Some(b) => {
                let s = std::str::from_utf8(b).map_err(|_| StoreError::NotInteger {
                    key: format!("{key}.{field}"),
                })?;
                let cur: i64 = s.parse().map_err(|_| StoreError::NotInteger {
                    key: format!("{key}.{field}"),
                })?;
                let next = cur.checked_add(by).ok_or(StoreError::IncrOverflow {
                    key: format!("{key}.{field}"),
                })?;
                h.insert(field.to_string(), next.to_string().into_bytes());
                Ok(next)
            }
        },
        other => Err(wrong_type(key, other.kind(), "hash")),
    }
}

/// Read all field/value pairs of a hash. Mirrors Valkey's HGETALL.
pub fn hash_get_all(store: &MemStore, key: &str) -> Result<HashMap<String, Vec<u8>>, StoreError> {
    match store.get(key) {
        None => Ok(HashMap::new()),
        Some(MemValue::Hash(h)) => Ok(h.clone()),
        Some(other) => Err(wrong_type(key, other.kind(), "hash")),
    }
}

/// List all members of a set. Mirrors Valkey's SMEMBERS. Returned in
/// undefined order.
pub fn set_members(store: &MemStore, key: &str) -> Result<Vec<String>, StoreError> {
    match store.get(key) {
        None => Ok(Vec::new()),
        Some(MemValue::Set(s)) => Ok(s.iter().cloned().collect()),
        Some(other) => Err(wrong_type(key, other.kind(), "set")),
    }
}

// -- Set ops --

pub fn set_add(store: &mut MemStore, key: String, member: String) -> Result<(), StoreError> {
    match store.entry(key.clone()) {
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
            other => Err(wrong_type(&key, other.kind(), "set")),
        },
    }
}

pub fn set_remove(store: &mut MemStore, key: &str, member: &str) -> Result<(), StoreError> {
    match store.get_mut(key) {
        None => Ok(()),
        Some(MemValue::Set(s)) => {
            s.remove(member);
            Ok(())
        }
        Some(other) => Err(wrong_type(key, other.kind(), "set")),
    }
}

pub fn set_contains(store: &MemStore, key: &str, member: &str) -> Result<bool, StoreError> {
    match store.get(key) {
        None => Ok(false),
        Some(MemValue::Set(s)) => Ok(s.contains(member)),
        Some(other) => Err(wrong_type(key, other.kind(), "set")),
    }
}
