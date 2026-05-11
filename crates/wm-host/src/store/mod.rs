//! Storage abstraction.
//!
//! `Storage` is the host-side factory that produces `Bucket` instances. Each
//! bucket is namespaced by an internal key prefix per `storage-model.md`:
//!
//!   route bucket: `kv:{group_ulid}:{route_ulid}:`
//!   group bucket: `gkv:{group_ulid}:`
//!
//! Buckets are owned per-request (held by the wasmtime resource table) and
//! act as thin views over the shared backing store. Persistence lives in the
//! backend, not in the bucket.

use std::sync::{Arc, Mutex};

use thiserror::Error;

use self::memory::MemStore;

pub mod memory;
// Always compiled: the case_ helpers and `storage_cases!` macro live here so
// the tier-3 integration test in `tests/valkey_storage.rs` can reuse them.
// The `in_memory` runner inside is itself `#[cfg(test)]`.
pub mod tests;
pub mod valkey;

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
    #[error("BACKEND: {0}")]
    Backend(String),
}

// -- Storage ------------------------------------------------------------------

/// Host-side storage factory. Cheap to clone (the inner state is `Arc`-ed).
#[derive(Clone)]
pub enum Storage {
    InMemory(Arc<Mutex<MemStore>>),
    Valkey(Arc<redis::Client>),
}

impl Storage {
    pub fn in_memory() -> Self {
        Self::InMemory(Arc::new(Mutex::new(MemStore::new())))
    }

    /// Connect to a Valkey instance and return a `Storage` backed by it.
    /// `url` follows the redis URL scheme: `redis://[user:pass@]host[:port][/db]`
    /// or `rediss://...` for TLS. The client is constructed eagerly so a bad
    /// URL fails here rather than on the first request.
    pub fn valkey(url: &str) -> Result<Self, StoreError> {
        let client = redis::Client::open(url).map_err(|e| StoreError::Backend(format!("{e}")))?;
        // Round-trip a PING so config errors surface at startup rather than
        // on the first mock request. Aligns with the project's fail-fast
        // preference for missing/invalid backend config.
        let mut conn = client
            .get_connection()
            .map_err(|e| StoreError::Backend(format!("connect: {e}")))?;
        let _: String = redis::cmd("PING")
            .query(&mut conn)
            .map_err(|e| StoreError::Backend(format!("ping: {e}")))?;
        Ok(Self::Valkey(Arc::new(client)))
    }

    /// Open the route-private bucket scoped to `(group, route)`.
    pub fn route_bucket(&self, group_ulid: &str, route_ulid: &str) -> Result<Bucket, StoreError> {
        self.route_bucket_under("", group_ulid, route_ulid)
    }

    /// Open the group-shared bucket scoped to `group`.
    pub fn group_bucket(&self, group_ulid: &str) -> Result<Bucket, StoreError> {
        self.group_bucket_under("", group_ulid)
    }

    /// Open the route-private bucket under a custom root prefix. With
    /// `root = ""` this is identical to `route_bucket`; the dry-run
    /// path passes `"dryrun:{run_id}:"` so handler reads and writes
    /// land in a per-run namespace that gets discarded on completion.
    pub fn route_bucket_under(
        &self,
        root: &str,
        group_ulid: &str,
        route_ulid: &str,
    ) -> Result<Bucket, StoreError> {
        let prefix = format!("{root}kv:{group_ulid}:{route_ulid}:");
        self.bucket_with_prefix(prefix)
    }

    /// Open the group-shared bucket under a custom root prefix. See
    /// `route_bucket_under` for context.
    pub fn group_bucket_under(&self, root: &str, group_ulid: &str) -> Result<Bucket, StoreError> {
        let prefix = format!("{root}gkv:{group_ulid}:");
        self.bucket_with_prefix(prefix)
    }

    /// Deep-copy every key under `src_prefix` (storage-level, no bucket
    /// prefix prepended) to the same suffix under `dst_prefix`. Used
    /// by the dry-run path to snapshot a route + group's state into a
    /// disposable namespace.
    pub fn copy_keys_with_prefix(
        &self,
        src_prefix: &str,
        dst_prefix: &str,
    ) -> Result<u64, StoreError> {
        match self {
            Storage::InMemory(store) => {
                let mut store = store.lock().expect("poisoned");
                Ok(memory::copy_with_prefix(&mut store, src_prefix, dst_prefix))
            }
            Storage::Valkey(client) => {
                let mut conn = client
                    .get_connection()
                    .map_err(|e| StoreError::Backend(format!("connect: {e}")))?;
                valkey::copy_with_prefix(&mut conn, src_prefix, dst_prefix)
            }
        }
    }

    /// Set a millisecond-precision TTL on every key under `prefix`.
    /// Used to mark the dry-run namespace for automatic Valkey reaping
    /// in case the host crashes between snapshot and cleanup. In-memory
    /// is a no-op (a restart wipes everything anyway).
    pub fn set_pttl_with_prefix(&self, prefix: &str, millis: u64) -> Result<(), StoreError> {
        match self {
            Storage::InMemory(_) => Ok(()),
            Storage::Valkey(client) => {
                let mut conn = client
                    .get_connection()
                    .map_err(|e| StoreError::Backend(format!("connect: {e}")))?;
                let keys = valkey::scan_with_prefix(&mut conn, prefix)?;
                for k in keys {
                    valkey::pexpire(&mut conn, &k, millis)?;
                }
                Ok(())
            }
        }
    }

    /// Open a bucket with no key prefix, for host-internal admin records
    /// (route table, group records, indexes). Handlers cannot reach this —
    /// the WIT contract only exposes per-route / per-group buckets.
    pub fn admin_bucket(&self) -> Result<Bucket, StoreError> {
        self.bucket_with_prefix(String::new())
    }

    /// Round-trip a no-op against the backend. Always Ok for in-memory;
    /// for Valkey, performs a `PING`. Used by `/__ready`.
    pub fn ping(&self) -> Result<(), StoreError> {
        match self {
            Storage::InMemory(_) => Ok(()),
            Storage::Valkey(client) => {
                let mut conn = client
                    .get_connection()
                    .map_err(|e| StoreError::Backend(format!("connect: {e}")))?;
                let _: String = redis::cmd("PING")
                    .query(&mut conn)
                    .map_err(|e| StoreError::Backend(format!("ping: {e}")))?;
                Ok(())
            }
        }
    }

    fn bucket_with_prefix(&self, prefix: String) -> Result<Bucket, StoreError> {
        match self {
            Storage::InMemory(store) => Ok(Bucket::InMemory {
                store: store.clone(),
                prefix,
            }),
            Storage::Valkey(client) => {
                let conn = client
                    .get_connection()
                    .map_err(|e| StoreError::Backend(format!("connect: {e}")))?;
                Ok(Bucket::Valkey { conn, prefix })
            }
        }
    }
}

// -- Bucket -------------------------------------------------------------------

/// Per-request handle on a scoped slice of the backing store. Maps to the
/// `bucket` resource in the WIT contract.
pub enum Bucket {
    InMemory {
        store: Arc<Mutex<MemStore>>,
        prefix: String,
    },
    Valkey {
        conn: redis::Connection,
        prefix: String,
    },
}

impl Bucket {
    // -- Basic KV --

    pub fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::get(&store, &fk).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::get(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn set(&mut self, key: &str, value: Vec<u8>) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::set(&mut store, fk, value);
                Ok(())
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::set(conn, &fk, value).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::delete(&mut store, &fk);
                Ok(())
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::delete(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    /// Set a TTL (in seconds) on `key`. No-op for the in-memory backend
    /// because tests don't need wall-clock-driven expiry; the journal /
    /// session lifecycles fall back to manual cleanup or the natural
    /// end of the test process. Valkey runs `EXPIRE`. The key must
    /// already exist; `EXPIRE` on a missing key returns 0 in Valkey
    /// and is silently ignored here.
    pub fn set_ttl(&mut self, key: &str, ttl_seconds: u64) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { .. } => Ok(()),
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                let _: i64 = redis::cmd("EXPIRE")
                    .arg(&fk)
                    .arg(ttl_seconds)
                    .query(conn)
                    .map_err(|e| StoreError::Backend(format!("EXPIRE: {e}")))?;
                Ok(())
            }
        }
    }

    /// Delete every key under `user_prefix`. Implemented as `SCAN +
    /// DEL` because Valkey has no native prefix-delete; on the
    /// in-memory backend it walks the map. Used by group lifecycle
    /// cleanup (cascade-delete and `DELETE /__api/groups/{group}/state`)
    /// where we need to wipe a whole namespace at once. Returns the
    /// number of keys actually deleted.
    pub fn delete_with_prefix(&mut self, user_prefix: &str) -> Result<u64, StoreError> {
        let keys = self.list_keys(Some(user_prefix))?;
        let count = keys.len() as u64;
        for key in keys {
            self.delete(&key)?;
        }
        Ok(count)
    }

    pub fn incr(&mut self, key: &str, by: i64) -> Result<i64, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::incr(&mut store, &fk, by).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::incr(conn, &fk, by).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    /// Storage-level type of `key`, or `None` when the key is absent.
    /// Returns `"bytes"`, `"list"`, `"hash"`, `"set"` (or `"other"` for
    /// a co-resident application's exotic types on Valkey). Used by
    /// state-inspection endpoints to label keys whose typed value
    /// won't fit in a generic `Vec<u8>` field.
    pub fn kind(&mut self, key: &str) -> Result<Option<&'static str>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                Ok(memory::kind(&store, &fk))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::kind(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn list_keys(&mut self, user_prefix: Option<&str>) -> Result<Vec<String>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let combined = match user_prefix {
                    Some(p) => format!("{prefix}{p}"),
                    None => prefix.clone(),
                };
                let store = store.lock().expect("poisoned");
                Ok(memory::scan_with_prefix(&store, &combined)
                    .into_iter()
                    .map(|k| k.strip_prefix(prefix.as_str()).unwrap_or(&k).to_string())
                    .collect())
            }
            Bucket::Valkey { conn, prefix } => {
                let combined = match user_prefix {
                    Some(p) => format!("{prefix}{p}"),
                    None => prefix.clone(),
                };
                let bucket_prefix = prefix.clone();
                Ok(valkey::scan_with_prefix(conn, &combined)?
                    .into_iter()
                    .map(|k| {
                        k.strip_prefix(bucket_prefix.as_str())
                            .unwrap_or(&k)
                            .to_string()
                    })
                    .collect())
            }
        }
    }

    // -- List ops --

    pub fn list_push(&mut self, key: &str, value: Vec<u8>) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::list_push(&mut store, fk, value).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::list_push(conn, &fk, value).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn list_pop(&mut self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::list_pop(&mut store, &fk).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::list_pop(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn list_range(
        &mut self,
        key: &str,
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::list_range(&store, &fk, start, stop)
                    .map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::list_range(conn, &fk, start, stop).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn list_length(&mut self, key: &str) -> Result<u64, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::list_length(&store, &fk).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::list_length(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    // -- Hash ops --

    pub fn hash_get(&mut self, key: &str, field: &str) -> Result<Option<Vec<u8>>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::hash_get(&store, &fk, field).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::hash_get(conn, &fk, field).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn hash_set(&mut self, key: &str, field: &str, value: Vec<u8>) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::hash_set(&mut store, fk, field.to_string(), value)
                    .map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::hash_set(conn, &fk, field, value).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn hash_delete(&mut self, key: &str, field: &str) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::hash_delete(&mut store, &fk, field).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::hash_delete(conn, &fk, field).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn hash_keys(&mut self, key: &str) -> Result<Vec<String>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::hash_keys(&store, &fk).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::hash_keys(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    /// Atomic increment of a hash field. Host-internal: not exposed via WIT
    /// (handlers should use `incr` for top-level counters). Used by the
    /// route registry to allocate per-group route numbers.
    pub fn hash_incr(&mut self, key: &str, field: &str, by: i64) -> Result<i64, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::hash_incr(&mut store, &fk, field, by)
                    .map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::hash_incr(conn, &fk, field, by).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    /// Read all field/value pairs of a hash. Host-internal: used by the
    /// registry to deserialise route/group records.
    pub fn hash_get_all(
        &mut self,
        key: &str,
    ) -> Result<std::collections::HashMap<String, Vec<u8>>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::hash_get_all(&store, &fk).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::hash_get_all(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    // -- Set ops --

    pub fn set_add(&mut self, key: &str, member: &str) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::set_add(&mut store, fk, member.to_string())
                    .map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::set_add(conn, &fk, member).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn set_remove(&mut self, key: &str, member: &str) -> Result<(), StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let mut store = store.lock().expect("poisoned");
                memory::set_remove(&mut store, &fk, member).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::set_remove(conn, &fk, member).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    pub fn set_contains(&mut self, key: &str, member: &str) -> Result<bool, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::set_contains(&store, &fk, member).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::set_contains(conn, &fk, member).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }

    /// List all members of a set. Host-internal: used by the registry to
    /// enumerate routes within a group.
    pub fn set_members(&mut self, key: &str) -> Result<Vec<String>, StoreError> {
        match self {
            Bucket::InMemory { store, prefix } => {
                let fk = format!("{prefix}{key}");
                let store = store.lock().expect("poisoned");
                memory::set_members(&store, &fk).map_err(|e| restore_user_key(e, prefix))
            }
            Bucket::Valkey { conn, prefix } => {
                let fk = format!("{prefix}{key}");
                valkey::set_members(conn, &fk).map_err(|e| restore_user_key(e, prefix))
            }
        }
    }
}

/// Backend ops report errors with the prefixed key. Strip the bucket prefix
/// so the error names the user-facing key, not the internal storage key.
fn restore_user_key(err: StoreError, prefix: &str) -> StoreError {
    match err {
        StoreError::WrongType {
            key,
            actual,
            expected,
        } => StoreError::WrongType {
            key: strip(&key, prefix),
            actual,
            expected,
        },
        StoreError::NotInteger { key } => StoreError::NotInteger {
            key: strip(&key, prefix),
        },
        StoreError::IncrOverflow { key } => StoreError::IncrOverflow {
            key: strip(&key, prefix),
        },
        StoreError::Backend(_) => err,
    }
}

fn strip(full: &str, prefix: &str) -> String {
    full.strip_prefix(prefix).unwrap_or(full).to_string()
}
