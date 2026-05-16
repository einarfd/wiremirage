//! Per-request journal: agent-debugging surface for mock traffic.
//!
//! Distinct from OTel observability (slice 6, ops audience) — this is
//! the product surface that answers "did the SUT call my mock, and what
//! happened?" for users and agents driving WireMirage. See
//! `storage-model.md` "Logs and trace IDs" + ADR-0017 for the split.
//!
//! Storage layout per `storage-model.md`:
//!
//!   journal:{group_ulid}:{ulid}            JSON-encoded JournalRecord, with TTL
//!   journal:by-number:{group_ulid}:{n}     string -> journal ulid (per-group sequence)
//!
//!   unmatched:{ulid}                       JSON-encoded UnmatchedRecord, with TTL
//!   unmatched:by-number:{n}                string -> unmatched ulid (host-wide)
//!   unmatched:counter                      i64 — host-wide allocator
//!
//! Per-group journal numbers are allocated from `group:counters:{group_ulid}`
//! (the same hash that holds `next_route_number`), keeping the counters
//! co-located with their parent group.
//!
//! Body truncation is applied at write time (16 KiB for handled, 4 KiB for
//! unmatched). Both records carry `body_truncated: bool` and
//! `original_body_size: usize` so consumers can flag what they're missing.
//!
//! TTL defaults: 1h handled, 1h unmatched. Hardcoded for slice 7 — env-var
//! configurable later. The in-memory backend treats `set_ttl` as a no-op,
//! so test runs accumulate records until the process exits; tier-3 Valkey
//! tests verify real TTL behavior when needed.
//!
//! Slice 7 scope: write path on every dispatched request, simple list/get
//! REST endpoints, cursor pagination, no SSE tail, no near-misses. Each
//! lands as a separate follow-up.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use ulid::Ulid;

use crate::store::{Bucket, Storage, StoreError};

const HANDLED_TTL_SECONDS: u64 = 3600;
const UNMATCHED_TTL_SECONDS: u64 = 3600;

/// Bus capacity for live-tail subscribers. A slow consumer that lags
/// further than this loses events (`broadcast` returns `Lagged(n)`),
/// which subscribers surface back to their callers. 256 is enough to
/// absorb typical dispatch bursts without making the channel a memory
/// hazard; revisit if real workloads stretch it.
const JOURNAL_BUS_CAPACITY: usize = 256;

/// Truncation cap for journal request and response bodies. Entries
/// larger than this are stored truncated with `body_truncated = true`
/// and the original size preserved in `original_body_size`.
pub const HANDLED_BODY_LIMIT: usize = 16 * 1024;
/// Same idea for the unmatched-request log; smaller because we don't
/// need as much body for a 404'd request.
pub const UNMATCHED_BODY_LIMIT: usize = 4 * 1024;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("storage backend error: {0}")]
    Storage(#[from] StoreError),
    #[error("journal record not found")]
    NotFound,
    #[error("malformed record in storage: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct JournalRecord {
    pub id: String,
    pub number: u32,
    /// W3C trace-id as 32 lowercase hex chars when known, `None`
    /// otherwise. `None` here means no inbound `traceparent` and OTel
    /// span export is disabled — there's no usable trace to correlate
    /// against.
    pub trace_id: Option<String>,
    pub group_id: String,
    pub group_name: String,
    pub route_id: String,
    pub route_number: u32,
    pub matched_pattern: String,
    pub request: RequestEnvelope,
    pub response: ResponseEnvelope,
    pub path_params: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub handler_logs: Vec<HandlerLogEntry>,
    pub duration_ms: u64,
    pub resources: ResourceUsage,
    pub error: Option<String>,
    pub dropped_response_headers: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RequestEnvelope {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub original_body_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ResponseEnvelope {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub original_body_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct HandlerLogEntry {
    pub level: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ResourceUsage {
    /// Wasmtime fuel consumed by the handler. Always 0 until per-route
    /// resource limits land — schema kept stable so consumers don't
    /// have to migrate later.
    pub fuel_consumed: u64,
    /// Peak memory usage. Same caveat as `fuel_consumed`.
    pub memory_peak_bytes: u64,
    pub wall_clock_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct UnmatchedRecord {
    pub id: String,
    pub number: u64,
    pub trace_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub request: RequestEnvelope,
    /// Routes that almost matched (same path, different method, or
    /// a one-segment string-prefix difference). Empty when no
    /// near-misses were detected. Populated by the dispatcher at
    /// unmatched-write time via `RouteTable::compute_near_misses`.
    #[serde(default)]
    pub near_misses: Vec<UnmatchedNearMiss>,
}

/// A nearby route attached to an `UnmatchedRecord`. Slim projection
/// of `route_table::NearMiss` — keeps the route's slug + path +
/// methods so the UI / agent can render the suggestion without a
/// follow-up lookup, but drops the heavy `compiled_wasm` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct UnmatchedNearMiss {
    /// `{group}/{number}` slug.
    pub route: String,
    pub route_path: String,
    pub route_methods: Vec<String>,
    pub reason: UnmatchedNearMissReason,
}

/// Why the route nearly-but-didn't match. Same two flavours the
/// slice-13 `find_route` MCP tool surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnmatchedNearMissReason {
    /// Pattern matched, methods didn't.
    MethodMismatch {
        expected_methods: Vec<String>,
        got: String,
    },
    /// One path segment differs by a literal string prefix.
    PrefixMatch {
        segment_index: usize,
        expected: String,
        got: String,
    },
}

/// Inputs the dispatcher hands to the journal at write time. Owned so
/// `record_handled` can move the body bytes into the persisted record
/// without an extra clone.
#[derive(Debug, Clone)]
pub struct NewJournalEntry {
    pub trace_id: Option<String>,
    pub group_id: String,
    pub group_name: String,
    pub route_id: String,
    pub route_number: u32,
    pub matched_pattern: String,
    pub request: RequestEnvelope,
    pub response: ResponseEnvelope,
    pub path_params: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub handler_logs: Vec<HandlerLogEntry>,
    pub duration_ms: u64,
    pub resources: ResourceUsage,
    pub error: Option<String>,
    pub dropped_response_headers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NewUnmatchedEntry {
    pub trace_id: Option<String>,
    pub request: RequestEnvelope,
    /// Computed by the dispatcher (or the test driver) at write
    /// time. Empty when nothing nearby was found, or when the
    /// caller doesn't have a route table to consult.
    #[doc(hidden)]
    pub near_misses: Vec<UnmatchedNearMiss>,
}

#[derive(Debug, Clone, Copy)]
pub struct ListCursor {
    /// Return entries whose `number` is strictly less than this. `None`
    /// means start from the newest.
    pub before: Option<u32>,
    /// Cap on entries returned. The journal clamps to `MAX_LIMIT`.
    pub limit: usize,
}

impl Default for ListCursor {
    fn default() -> Self {
        Self {
            before: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UnmatchedCursor {
    pub before: Option<u64>,
    pub limit: usize,
}

impl Default for UnmatchedCursor {
    fn default() -> Self {
        Self {
            before: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 100;

/// Truncate `body` to at most `limit` bytes, returning the truncated
/// vector along with the original size and a flag.
pub fn truncate_body(body: Vec<u8>, limit: usize) -> (Vec<u8>, usize, bool) {
    let original_size = body.len();
    if original_size <= limit {
        (body, original_size, false)
    } else {
        let mut truncated = body;
        truncated.truncate(limit);
        (truncated, original_size, true)
    }
}

/// One event on the live-tail bus. Mirrors the two record shapes
/// `Journal` produces — handled requests and unmatched requests —
/// because consumers (the SSE endpoint, the MCP streaming tools) are
/// interested in the same events the journal already stores.
///
/// Single-host fan-out only: the bus is in-process, so sibling hosts
/// in a multi-host deployment won't observe each other's events. A
/// follow-up slice can put Valkey pub/sub behind the same shape if
/// multi-host becomes real.
///
/// Variants are boxed so cloning around the broadcast bus is cheap —
/// a `JournalRecord` is ~500 bytes, and the channel capacity is 256.
#[derive(Debug, Clone)]
pub enum JournalEvent {
    Handled(Box<JournalRecord>),
    Unmatched(Box<UnmatchedRecord>),
}

#[derive(Clone)]
pub struct Journal {
    storage: Storage,
    bus: broadcast::Sender<JournalEvent>,
}

impl Journal {
    pub fn new(storage: Storage) -> Self {
        let (bus, _) = broadcast::channel(JOURNAL_BUS_CAPACITY);
        Self { storage, bus }
    }

    fn bucket(&self) -> Result<Bucket, JournalError> {
        Ok(self.storage.admin_bucket()?)
    }

    /// Subscribe to live-tail events. The subscriber is responsible
    /// for handling `Lagged` errors — a slow consumer can fall behind
    /// the bus capacity.
    pub fn subscribe(&self) -> broadcast::Receiver<JournalEvent> {
        self.bus.subscribe()
    }

    /// Publish helper. `broadcast::Sender::send` returns `Err` only
    /// when there are zero active receivers, which is normal — the
    /// bus is best-effort and we drop events silently in that case.
    fn publish(&self, event: JournalEvent) {
        let _ = self.bus.send(event);
    }

    // -- Handled requests ---------------------------------------------------

    pub fn record_handled(&self, entry: NewJournalEntry) -> Result<JournalRecord, JournalError> {
        let mut bucket = self.bucket()?;
        let n = bucket.hash_incr(
            &format!("group:counters:{}", entry.group_id),
            "next_journal_number",
            1,
        )? as u32;
        let id = Ulid::new().to_string();
        let record = JournalRecord {
            id: id.clone(),
            number: n,
            trace_id: entry.trace_id,
            group_id: entry.group_id,
            group_name: entry.group_name,
            route_id: entry.route_id,
            route_number: entry.route_number,
            matched_pattern: entry.matched_pattern,
            request: entry.request,
            response: entry.response,
            path_params: entry.path_params,
            query: entry.query,
            handler_logs: entry.handler_logs,
            duration_ms: entry.duration_ms,
            resources: entry.resources,
            error: entry.error,
            dropped_response_headers: entry.dropped_response_headers,
            created_at: Utc::now(),
        };
        let key = format!("journal:{}:{}", record.group_id, record.id);
        let json = serde_json::to_vec(&record)
            .map_err(|e| JournalError::Malformed(format!("encode: {e}")))?;
        bucket.set(&key, json)?;
        bucket.set_ttl(&key, HANDLED_TTL_SECONDS)?;
        bucket.set(
            &format!("journal:by-number:{}:{}", record.group_id, record.number),
            record.id.as_bytes().to_vec(),
        )?;
        self.publish(JournalEvent::Handled(Box::new(record.clone())));
        Ok(record)
    }

    pub fn get(&self, group_id: &str, number: u32) -> Result<JournalRecord, JournalError> {
        let mut bucket = self.bucket()?;
        let id_bytes = bucket
            .get(&format!("journal:by-number:{group_id}:{number}"))?
            .ok_or(JournalError::NotFound)?;
        let id = String::from_utf8(id_bytes)
            .map_err(|_| JournalError::Malformed("journal:by-number value".into()))?;
        self.read_by_id(&mut bucket, group_id, &id)
    }

    pub fn list_for_group(
        &self,
        group_id: &str,
        cursor: ListCursor,
    ) -> Result<Vec<JournalRecord>, JournalError> {
        let mut bucket = self.bucket()?;
        // Determine the highest journal number issued for this group;
        // walk down from `before` (or the highest) until we've collected
        // `limit` records or run out.
        let highest = bucket
            .hash_get(&format!("group:counters:{group_id}"), "next_journal_number")?
            .map(|bytes| {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let starting = match cursor.before {
            Some(b) => b.saturating_sub(1),
            None => highest,
        };
        if starting == 0 {
            return Ok(Vec::new());
        }
        let limit = cursor.limit.clamp(1, MAX_LIMIT);
        let mut out = Vec::with_capacity(limit);
        let mut n = starting;
        while out.len() < limit && n > 0 {
            if let Ok(record) = self.get_via_bucket(&mut bucket, group_id, n) {
                out.push(record);
            }
            // Numbers can have gaps when records expire / fail to read;
            // walk down and skip silently.
            n = n.saturating_sub(1);
        }
        Ok(out)
    }

    fn get_via_bucket(
        &self,
        bucket: &mut Bucket,
        group_id: &str,
        number: u32,
    ) -> Result<JournalRecord, JournalError> {
        let id_bytes = bucket
            .get(&format!("journal:by-number:{group_id}:{number}"))?
            .ok_or(JournalError::NotFound)?;
        let id = String::from_utf8(id_bytes)
            .map_err(|_| JournalError::Malformed("journal:by-number value".into()))?;
        self.read_by_id(bucket, group_id, &id)
    }

    fn read_by_id(
        &self,
        bucket: &mut Bucket,
        group_id: &str,
        id: &str,
    ) -> Result<JournalRecord, JournalError> {
        let bytes = bucket
            .get(&format!("journal:{group_id}:{id}"))?
            .ok_or(JournalError::NotFound)?;
        serde_json::from_slice(&bytes).map_err(|e| JournalError::Malformed(format!("decode: {e}")))
    }

    // -- Unmatched requests --------------------------------------------------

    pub fn record_unmatched(
        &self,
        entry: NewUnmatchedEntry,
    ) -> Result<UnmatchedRecord, JournalError> {
        let mut bucket = self.bucket()?;
        let n = bucket.incr("unmatched:counter", 1)? as u64;
        let id = Ulid::new().to_string();
        let record = UnmatchedRecord {
            id: id.clone(),
            number: n,
            trace_id: entry.trace_id,
            created_at: Utc::now(),
            request: entry.request,
            near_misses: entry.near_misses,
        };
        let key = format!("unmatched:{}", record.id);
        let json = serde_json::to_vec(&record)
            .map_err(|e| JournalError::Malformed(format!("encode: {e}")))?;
        bucket.set(&key, json)?;
        bucket.set_ttl(&key, UNMATCHED_TTL_SECONDS)?;
        bucket.set(
            &format!("unmatched:by-number:{}", record.number),
            record.id.as_bytes().to_vec(),
        )?;
        self.publish(JournalEvent::Unmatched(Box::new(record.clone())));
        Ok(record)
    }

    pub fn get_unmatched(&self, number: u64) -> Result<UnmatchedRecord, JournalError> {
        let mut bucket = self.bucket()?;
        let id_bytes = bucket
            .get(&format!("unmatched:by-number:{number}"))?
            .ok_or(JournalError::NotFound)?;
        let id = String::from_utf8(id_bytes)
            .map_err(|_| JournalError::Malformed("unmatched:by-number value".into()))?;
        self.read_unmatched_by_id(&mut bucket, &id)
    }

    pub fn list_unmatched(
        &self,
        cursor: UnmatchedCursor,
    ) -> Result<Vec<UnmatchedRecord>, JournalError> {
        let mut bucket = self.bucket()?;
        let highest = bucket
            .get("unmatched:counter")?
            .and_then(|bytes| {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .unwrap_or(0);
        let starting = match cursor.before {
            Some(b) => b.saturating_sub(1),
            None => highest,
        };
        if starting == 0 {
            return Ok(Vec::new());
        }
        let limit = cursor.limit.clamp(1, MAX_LIMIT);
        let mut out = Vec::with_capacity(limit);
        let mut n = starting;
        while out.len() < limit && n > 0 {
            if let Ok(record) = self.get_unmatched_via_bucket(&mut bucket, n) {
                out.push(record);
            }
            n = n.saturating_sub(1);
        }
        Ok(out)
    }

    fn get_unmatched_via_bucket(
        &self,
        bucket: &mut Bucket,
        number: u64,
    ) -> Result<UnmatchedRecord, JournalError> {
        let id_bytes = bucket
            .get(&format!("unmatched:by-number:{number}"))?
            .ok_or(JournalError::NotFound)?;
        let id = String::from_utf8(id_bytes)
            .map_err(|_| JournalError::Malformed("unmatched:by-number value".into()))?;
        self.read_unmatched_by_id(bucket, &id)
    }

    fn read_unmatched_by_id(
        &self,
        bucket: &mut Bucket,
        id: &str,
    ) -> Result<UnmatchedRecord, JournalError> {
        let bytes = bucket
            .get(&format!("unmatched:{id}"))?
            .ok_or(JournalError::NotFound)?;
        serde_json::from_slice(&bytes).map_err(|e| JournalError::Malformed(format!("decode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Journal {
        Journal::new(Storage::in_memory())
    }

    fn sample_envelope(body: &[u8]) -> RequestEnvelope {
        RequestEnvelope {
            method: "POST".into(),
            path: "/v1/charges".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_vec(),
            body_truncated: false,
            original_body_size: body.len(),
        }
    }

    fn sample_response(body: &[u8]) -> ResponseEnvelope {
        ResponseEnvelope {
            status: 200,
            headers: vec![],
            body: body.to_vec(),
            body_truncated: false,
            original_body_size: body.len(),
        }
    }

    fn sample_entry(group: &str) -> NewJournalEntry {
        NewJournalEntry {
            trace_id: Some("0123456789abcdef0123456789abcdef".into()),
            group_id: group.to_string(),
            group_name: "stripe-mock".into(),
            route_id: "01HZZ".into(),
            route_number: 1,
            matched_pattern: "/v1/charges".into(),
            request: sample_envelope(b"{}"),
            response: sample_response(b"{\"ok\":true}"),
            path_params: vec![],
            query: vec![],
            handler_logs: vec![],
            duration_ms: 12,
            resources: ResourceUsage::default(),
            error: None,
            dropped_response_headers: vec![],
        }
    }

    #[test]
    fn handled_record_round_trip() {
        let j = fresh();
        let written = j.record_handled(sample_entry("g1")).unwrap();
        assert_eq!(written.number, 1);
        let read = j.get("g1", 1).unwrap();
        assert_eq!(read, written);
    }

    #[test]
    fn handled_numbers_are_per_group() {
        let j = fresh();
        let r1 = j.record_handled(sample_entry("g1")).unwrap();
        let r2 = j.record_handled(sample_entry("g2")).unwrap();
        let r3 = j.record_handled(sample_entry("g1")).unwrap();
        assert_eq!(r1.number, 1);
        assert_eq!(r2.number, 1);
        assert_eq!(r3.number, 2);
    }

    #[test]
    fn list_returns_newest_first_capped_at_limit() {
        let j = fresh();
        for _ in 0..5 {
            j.record_handled(sample_entry("g1")).unwrap();
        }
        let page = j
            .list_for_group(
                "g1",
                ListCursor {
                    before: None,
                    limit: 3,
                },
            )
            .unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].number, 5);
        assert_eq!(page[1].number, 4);
        assert_eq!(page[2].number, 3);
    }

    #[test]
    fn list_cursor_pagination() {
        let j = fresh();
        for _ in 0..5 {
            j.record_handled(sample_entry("g1")).unwrap();
        }
        let first = j
            .list_for_group(
                "g1",
                ListCursor {
                    before: None,
                    limit: 2,
                },
            )
            .unwrap();
        assert_eq!(
            first.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![5, 4]
        );
        let next = j
            .list_for_group(
                "g1",
                ListCursor {
                    before: Some(4),
                    limit: 2,
                },
            )
            .unwrap();
        assert_eq!(
            next.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![3, 2]
        );
        let tail = j
            .list_for_group(
                "g1",
                ListCursor {
                    before: Some(2),
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(tail.iter().map(|r| r.number).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn list_empty_group_is_empty() {
        let j = fresh();
        let out = j.list_for_group("g1", ListCursor::default()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn get_missing_is_not_found() {
        let j = fresh();
        let err = j.get("g1", 7).unwrap_err();
        assert!(matches!(err, JournalError::NotFound));
    }

    #[test]
    fn unmatched_record_round_trip() {
        let j = fresh();
        let written = j
            .record_unmatched(NewUnmatchedEntry {
                trace_id: None,
                request: sample_envelope(b"oops"),
                near_misses: Vec::new(),
            })
            .unwrap();
        assert_eq!(written.number, 1);
        assert!(written.near_misses.is_empty());
        let read = j.get_unmatched(1).unwrap();
        assert_eq!(read, written);
    }

    #[test]
    fn unmatched_numbers_are_host_wide() {
        let j = fresh();
        let r1 = j
            .record_unmatched(NewUnmatchedEntry {
                trace_id: None,
                request: sample_envelope(b""),
                near_misses: Vec::new(),
            })
            .unwrap();
        let r2 = j
            .record_unmatched(NewUnmatchedEntry {
                trace_id: None,
                request: sample_envelope(b""),
                near_misses: Vec::new(),
            })
            .unwrap();
        assert_eq!(r1.number, 1);
        assert_eq!(r2.number, 2);
    }

    #[test]
    fn unmatched_list_with_cursor() {
        let j = fresh();
        for _ in 0..4 {
            j.record_unmatched(NewUnmatchedEntry {
                trace_id: None,
                request: sample_envelope(b""),
                near_misses: Vec::new(),
            })
            .unwrap();
        }
        let page = j
            .list_unmatched(UnmatchedCursor {
                before: None,
                limit: 2,
            })
            .unwrap();
        assert_eq!(
            page.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![4, 3]
        );
        let next = j
            .list_unmatched(UnmatchedCursor {
                before: Some(3),
                limit: 5,
            })
            .unwrap();
        assert_eq!(
            next.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    #[test]
    fn truncate_body_above_limit_marks_truncated() {
        let body = vec![b'x'; HANDLED_BODY_LIMIT * 2];
        let (out, original, truncated) = truncate_body(body, HANDLED_BODY_LIMIT);
        assert!(truncated);
        assert_eq!(out.len(), HANDLED_BODY_LIMIT);
        assert_eq!(original, HANDLED_BODY_LIMIT * 2);
    }

    #[test]
    fn truncate_body_below_limit_passes_through() {
        let body = vec![b'x'; 16];
        let (out, original, truncated) = truncate_body(body.clone(), HANDLED_BODY_LIMIT);
        assert!(!truncated);
        assert_eq!(out, body);
        assert_eq!(original, 16);
    }
}
