//! Valkey-backed storage operations. Each helper takes a borrowed
//! `redis::Connection` and operates on a fully-prefixed key — the locking
//! and prefix logic lives in `Bucket::Valkey` in the parent module.
//!
//! All ops use the sync `redis` API; the wasmtime call path is already on a
//! `spawn_blocking` thread, so blocking RESP I/O here is fine.

use redis::Commands;

use super::StoreError;

fn classify(err: redis::RedisError, key: &str) -> StoreError {
    if err.code() == Some("WRONGTYPE") {
        return StoreError::WrongType {
            key: key.to_string(),
            actual: "unknown",
            expected: "unknown",
        };
    }
    StoreError::Backend(format!("{err}"))
}

// -- Basic key-value --

pub fn get(conn: &mut redis::Connection, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    conn.get(key).map_err(|e| classify(e, key))
}

pub fn set(conn: &mut redis::Connection, key: &str, value: Vec<u8>) -> Result<(), StoreError> {
    conn.set::<_, _, ()>(key, value)
        .map_err(|e| classify(e, key))
}

pub fn delete(conn: &mut redis::Connection, key: &str) -> Result<(), StoreError> {
    conn.del::<_, ()>(key).map_err(|e| classify(e, key))
}

pub fn incr(conn: &mut redis::Connection, key: &str, by: i64) -> Result<i64, StoreError> {
    conn.incr::<_, _, i64>(key, by).map_err(|err| {
        // Valkey returns ERR with text "value is not an integer or out of range"
        // for non-integer existing values OR for overflow. Disambiguate using
        // the message; we lose the explicit OVERFLOW path but the WIT contract
        // documents both as a trap with no user-visible distinction anyway.
        if err.code() == Some("WRONGTYPE") {
            return StoreError::WrongType {
                key: key.to_string(),
                actual: "unknown",
                expected: "bytes",
            };
        }
        let msg = format!("{err}");
        if msg.contains("not an integer") {
            return StoreError::NotInteger {
                key: key.to_string(),
            };
        }
        StoreError::Backend(msg)
    })
}

pub fn scan_with_prefix(
    conn: &mut redis::Connection,
    full_prefix: &str,
) -> Result<Vec<String>, StoreError> {
    let pattern = format!("{full_prefix}*");
    let iter = conn
        .scan_match::<_, String>(&pattern)
        .map_err(|e| classify(e, full_prefix))?;
    iter.collect::<redis::RedisResult<Vec<String>>>()
        .map_err(|e| classify(e, full_prefix))
}

// -- List ops --

pub fn list_push(
    conn: &mut redis::Connection,
    key: &str,
    value: Vec<u8>,
) -> Result<(), StoreError> {
    conn.rpush::<_, _, ()>(key, value)
        .map_err(|e| classify(e, key))
}

pub fn list_pop(conn: &mut redis::Connection, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    conn.lpop::<_, Option<Vec<u8>>>(key, None)
        .map_err(|e| classify(e, key))
}

pub fn list_range(
    conn: &mut redis::Connection,
    key: &str,
    start: i64,
    stop: i64,
) -> Result<Vec<Vec<u8>>, StoreError> {
    conn.lrange(key, start as isize, stop as isize)
        .map_err(|e| classify(e, key))
}

pub fn list_length(conn: &mut redis::Connection, key: &str) -> Result<u64, StoreError> {
    let n: i64 = conn.llen(key).map_err(|e| classify(e, key))?;
    Ok(n.max(0) as u64)
}

// -- Hash ops --

pub fn hash_get(
    conn: &mut redis::Connection,
    key: &str,
    field: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    conn.hget(key, field).map_err(|e| classify(e, key))
}

pub fn hash_set(
    conn: &mut redis::Connection,
    key: &str,
    field: &str,
    value: Vec<u8>,
) -> Result<(), StoreError> {
    conn.hset::<_, _, _, ()>(key, field, value)
        .map_err(|e| classify(e, key))
}

pub fn hash_delete(conn: &mut redis::Connection, key: &str, field: &str) -> Result<(), StoreError> {
    conn.hdel::<_, _, ()>(key, field)
        .map_err(|e| classify(e, key))
}

pub fn hash_keys(conn: &mut redis::Connection, key: &str) -> Result<Vec<String>, StoreError> {
    conn.hkeys(key).map_err(|e| classify(e, key))
}

// -- Set ops --

pub fn set_add(conn: &mut redis::Connection, key: &str, member: &str) -> Result<(), StoreError> {
    conn.sadd::<_, _, ()>(key, member)
        .map_err(|e| classify(e, key))
}

pub fn set_remove(conn: &mut redis::Connection, key: &str, member: &str) -> Result<(), StoreError> {
    conn.srem::<_, _, ()>(key, member)
        .map_err(|e| classify(e, key))
}

pub fn set_contains(
    conn: &mut redis::Connection,
    key: &str,
    member: &str,
) -> Result<bool, StoreError> {
    conn.sismember(key, member).map_err(|e| classify(e, key))
}
