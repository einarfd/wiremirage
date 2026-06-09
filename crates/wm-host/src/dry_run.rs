//! Dry-run dispatch: run a route's handler against a synthetic
//! request without journaling, isolating side effects in a per-run
//! shifted namespace that is discarded on completion.
//!
//! Semantics: the route's `kv:{group}:{route}:*` and the group's
//! `gkv:{group}:*` are deep-copied (preserving value type) under a
//! `dryrun:{run_id}:` root. The handler is instantiated with buckets
//! whose prefix points at the shifted root, so its reads see a
//! point-in-time snapshot and its writes land in the snapshot — never
//! the real state. After the handler returns (or traps), the
//! `dryrun:{run_id}:` namespace is wiped with one
//! `delete_with_prefix`. The journal is not touched.
//!
//! Crash safety: on Valkey, the shifted keys carry a 60-second
//! `PEXPIRE` so a host crash between snapshot and cleanup doesn't
//! leave orphans forever. In-memory storage clears on restart anyway.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::bindings::wiremirage::handler::http::Request as WitRequest;
use crate::log::LogRecord;
use crate::registry::Route;
use crate::route_table::RouteTable;
use crate::runtime::Runtime;
use crate::store::{Storage, StoreError};
use crate::wire::WireBytes;

/// Synthetic request shape posted to the dry-run endpoint. Mirrors
/// the WIT request type, with sensible defaults for fields most
/// callers won't bother setting (headers, body, query, path_params).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DryRunRequest {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, with = "crate::wire::bytes_field")]
    pub body: Vec<u8>,
    /// Override the path-params list the handler observes. When
    /// omitted, the handler sees an empty list (the dry-run path
    /// doesn't re-run the matcher; the caller supplied the slug and
    /// knows what they want).
    #[serde(default)]
    pub path_params: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub query: Vec<(String, String)>,
    /// Seed entries written into the route's private `kv:` namespace
    /// *after* the real-state deep-copy and *before* the handler
    /// runs. Lets a caller test state-dependent branches like
    /// `if counter > 3` without first hitting the route N times.
    /// Single-value-only (no list/set/hash seeding yet); collection-typed
    /// branches need the seed-via-real-traffic workaround for now.
    /// Values use the ADR-0025 [`WireBytes`] encoding (UTF-8 string,
    /// or `{base64}` for binary). Real `kv:` state is never touched —
    /// overrides land in the disposable snapshot.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub kv_overrides: HashMap<String, WireBytes>,
    /// Same as `kv_overrides`, scoped to the group's shared `gkv:`
    /// namespace.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub gkv_overrides: HashMap<String, WireBytes>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(with = "crate::wire::bytes_field")]
    pub body: Vec<u8>,
    pub handler_logs: Vec<DryRunLog>,
    pub duration_ms: u64,
    pub error: Option<String>,
    /// Number of keys snapshotted under the dry-run root (route's
    /// private kv + group's shared gkv). Helpful for confirming the
    /// route actually had state to test against.
    pub snapshot_keys: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunLog {
    pub level: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum DryRunError {
    #[error("storage error: {0}")]
    Storage(#[from] StoreError),
    #[error("join error: {0}")]
    Join(String),
}

/// Crash-safety TTL on the dry-run namespace. 60 seconds is long
/// enough for any reasonable handler to complete and short enough
/// that an orphaned namespace doesn't accumulate.
const DRY_RUN_TTL_MS: u64 = 60_000;

pub async fn dry_run(
    runtime: Arc<Runtime>,
    routes: Arc<RouteTable>,
    route: Route,
    req: DryRunRequest,
) -> Result<DryRunResponse, DryRunError> {
    let started = Instant::now();
    let run_id = Ulid::new().to_string();
    let dry_root = format!("dryrun:{run_id}:");
    let storage = runtime.storage().clone();

    // Snapshot + run. `cleanup_dry_run` runs unconditionally below.
    let outcome = run_in_snapshot(
        runtime.clone(),
        routes.clone(),
        storage.clone(),
        &route,
        &dry_root,
        req,
    )
    .await;

    // Cleanup: best-effort. The PEXPIRE we set during the snapshot
    // means Valkey will reap orphans even if this fails.
    if let Err(e) = cleanup_dry_run(&storage, &dry_root) {
        tracing::warn!(error = %e, run_id = %run_id, "dry-run cleanup failed");
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    Ok(match outcome {
        Ok(out) => DryRunResponse {
            status: out.status,
            headers: out.headers,
            body: out.body,
            handler_logs: out.logs.into_iter().map(into_dry_log).collect(),
            duration_ms,
            error: None,
            snapshot_keys: out.snapshot_keys,
        },
        Err(fail) => DryRunResponse {
            // Handler traps surface as 500-ish so the agent sees the
            // failure shape without needing a separate error channel.
            status: 500,
            headers: Vec::new(),
            body: fail.message.as_bytes().to_vec(),
            handler_logs: fail.logs.into_iter().map(into_dry_log).collect(),
            duration_ms,
            error: Some(fail.message),
            snapshot_keys: fail.snapshot_keys,
        },
    })
}

struct RunOk {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    logs: Vec<LogRecord>,
    snapshot_keys: u64,
}

struct RunFail {
    message: String,
    logs: Vec<LogRecord>,
    snapshot_keys: u64,
}

async fn run_in_snapshot(
    runtime: Arc<Runtime>,
    routes: Arc<RouteTable>,
    storage: Storage,
    route: &Route,
    dry_root: &str,
    req: DryRunRequest,
) -> Result<RunOk, RunFail> {
    // Snapshot the route's private kv + group's shared kv. Empty
    // namespaces are fine — copy_keys_with_prefix returns 0.
    let kv_src = format!("kv:{}:{}:", route.group_id, route.id);
    let kv_dst = format!("{dry_root}kv:{}:{}:", route.group_id, route.id);
    let gkv_src = format!("gkv:{}:", route.group_id);
    let gkv_dst = format!("{dry_root}gkv:{}:", route.group_id);

    let kv_copied = match storage.copy_keys_with_prefix(&kv_src, &kv_dst) {
        Ok(n) => n,
        Err(e) => {
            return Err(RunFail {
                message: format!("copy route kv: {e}"),
                logs: Vec::new(),
                snapshot_keys: 0,
            });
        }
    };
    let gkv_copied = match storage.copy_keys_with_prefix(&gkv_src, &gkv_dst) {
        Ok(n) => n,
        Err(e) => {
            return Err(RunFail {
                message: format!("copy group kv: {e}"),
                logs: Vec::new(),
                snapshot_keys: kv_copied,
            });
        }
    };
    let snapshot_keys = kv_copied + gkv_copied;

    if let Err(e) = storage.set_pttl_with_prefix(dry_root, DRY_RUN_TTL_MS) {
        tracing::warn!(error = %e, "dry-run safety TTL failed");
    }

    let mut route_bucket = match storage.route_bucket_under(dry_root, &route.group_id, &route.id) {
        Ok(b) => b,
        Err(e) => {
            return Err(RunFail {
                message: format!("open dry route bucket: {e}"),
                logs: Vec::new(),
                snapshot_keys,
            });
        }
    };
    let mut group_bucket = match storage.group_bucket_under(dry_root, &route.group_id) {
        Ok(b) => b,
        Err(e) => {
            return Err(RunFail {
                message: format!("open dry group bucket: {e}"),
                logs: Vec::new(),
                snapshot_keys,
            });
        }
    };
    // Apply seed-state overrides on top of the real-state deep-copy.
    // Order matters: overrides win, so a caller can flip `counter=4`
    // without resetting the rest of the route's state. Single-value
    // only; list/set/hash overrides land in a follow-up.
    for (k, v) in req.kv_overrides {
        let bytes = match v.into_bytes() {
            Ok(b) => b,
            Err(e) => return Err(override_decode_fail("kv", &k, e, snapshot_keys)),
        };
        if let Err(e) = route_bucket.set(&k, bytes) {
            return Err(RunFail {
                message: format!("apply kv override {k:?}: {e}"),
                logs: Vec::new(),
                snapshot_keys,
            });
        }
    }
    for (k, v) in req.gkv_overrides {
        let bytes = match v.into_bytes() {
            Ok(b) => b,
            Err(e) => return Err(override_decode_fail("gkv", &k, e, snapshot_keys)),
        };
        if let Err(e) = group_bucket.set(&k, bytes) {
            return Err(RunFail {
                message: format!("apply gkv override {k:?}: {e}"),
                logs: Vec::new(),
                snapshot_keys,
            });
        }
    }

    let wit_request = WitRequest {
        method: req.method.to_uppercase(),
        path: req.path.clone(),
        matched_pattern: route.path.clone(),
        path_params: req.path_params.unwrap_or_default(),
        query: req.query,
        headers: req
            .headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .collect(),
        body: req.body,
    };

    // Source-language routes (JS/TS) run through the shared engine,
    // exactly like dispatch — including streaming via
    // `host.responseStream`. Pre-compiled / AOT components run the
    // buffered handler-world path.
    let use_engine = matches!(route.language.as_str(), "javascript" | "typescript");
    if use_engine {
        return run_engine_in_snapshot(
            runtime,
            route,
            route_bucket,
            group_bucket,
            wit_request,
            snapshot_keys,
        )
        .await;
    }

    let component = match routes.component_for(route) {
        Ok(c) => c,
        Err(e) => {
            return Err(RunFail {
                message: format!("compile component: {e}"),
                logs: Vec::new(),
                snapshot_keys,
            });
        }
    };

    let runtime_for_task = runtime;
    let outcome = tokio::task::spawn_blocking(move || {
        let (handler, mut store, handles) =
            match runtime_for_task.instantiate_with_buckets(&component, route_bucket, group_bucket)
            {
                Ok(t) => t,
                Err(e) => return (Err(format!("{e:#}")), Vec::new()),
            };
        let result = handler.call_handle(&mut store, &wit_request, handles.route, handles.group);
        let logs = store.data_mut().take_logs();
        match result {
            Ok(resp) => (Ok(resp), logs),
            Err(e) => (Err(format!("{e:#}")), logs),
        }
    })
    .await;

    match outcome {
        Ok((Ok(wit_resp), logs)) => Ok(RunOk {
            status: wit_resp.status,
            headers: wit_resp.headers,
            body: wit_resp.body,
            logs,
            snapshot_keys,
        }),
        Ok((Err(msg), logs)) => Err(RunFail {
            message: msg,
            logs,
            snapshot_keys,
        }),
        Err(join_err) => Err(RunFail {
            message: format!("dry-run task join error: {join_err}"),
            logs: Vec::new(),
            snapshot_keys,
        }),
    }
}

/// Run a source-language (engine) route under the dry-run snapshot.
/// Mirrors the dispatch engine path but collects any streamed chunks
/// in-process (no real client) so the dry-run response carries the
/// full streamed body. A handler that streams via `host.responseStream`
/// has its head + concatenated chunks returned; a buffered handler
/// returns its response value as usual.
async fn run_engine_in_snapshot(
    runtime: Arc<Runtime>,
    route: &Route,
    route_bucket: crate::store::Bucket,
    group_bucket: crate::store::Bucket,
    wit_request: WitRequest,
    snapshot_keys: u64,
) -> Result<RunOk, RunFail> {
    // The stored `source` is the original author source (ADR-0020); resolve
    // the JS the engine runs (transpile TS, JS as-is). Dry-run is infrequent,
    // so transpile inline rather than touching the dispatch JS cache.
    let Some(original) = route.source.as_deref() else {
        return Err(RunFail {
            message: format!("source-language route {} has no source stored", route.id),
            logs: Vec::new(),
            snapshot_keys,
        });
    };
    let source = if route.language == "typescript" {
        match crate::ts_transpile::transpile(original) {
            Ok(js) => js,
            Err(e) => {
                return Err(RunFail {
                    message: format!("transpile route {}: {e}", route.id),
                    logs: Vec::new(),
                    snapshot_keys,
                });
            }
        }
    } else {
        original.to_string()
    };

    let (head_tx, mut head_rx) = tokio::sync::oneshot::channel::<crate::host_state::StreamHead>();
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    let handle = tokio::task::spawn_blocking(move || {
        let (engine_world, mut store, handles) =
            match runtime.instantiate_engine_with_buckets(source, route_bucket, group_bucket) {
                Ok(t) => t,
                Err(e) => return (Err(format!("{e:#}")), Vec::new()),
            };
        store.data_mut().set_response_stream_sink(head_tx, chunk_tx);
        let engine_req = crate::bindings::handler_request_to_engine(wit_request);
        let result = engine_world
            .call_handle(&mut store, &engine_req, handles.route, handles.group)
            .map(crate::bindings::engine_response_to_handler);
        let logs = store.data_mut().take_logs();
        match result {
            Ok(resp) => (Ok(resp), logs),
            Err(e) => (Err(format!("{e:#}")), logs),
        }
    });

    // Drain streamed chunks concurrently with the handler so its
    // `write-chunk` blocking-sends never wedge on a full channel.
    let mut streamed_body = Vec::new();
    while let Some(chunk) = chunk_rx.recv().await {
        streamed_body.extend_from_slice(&chunk);
    }

    let (result, logs) = match handle.await {
        Ok(t) => t,
        Err(join_err) => {
            return Err(RunFail {
                message: format!("dry-run task join error: {join_err}"),
                logs: Vec::new(),
                snapshot_keys,
            });
        }
    };

    match result {
        Ok(wit_resp) => {
            // Streamed (handler called `start`) → the head's status +
            // headers and the collected chunk body are the response.
            // Otherwise the buffered return value.
            if let Ok(head) = head_rx.try_recv() {
                Ok(RunOk {
                    status: head.status,
                    headers: head.headers,
                    body: streamed_body,
                    logs,
                    snapshot_keys,
                })
            } else {
                Ok(RunOk {
                    status: wit_resp.status,
                    headers: wit_resp.headers,
                    body: wit_resp.body,
                    logs,
                    snapshot_keys,
                })
            }
        }
        Err(msg) => Err(RunFail {
            message: msg,
            logs,
            snapshot_keys,
        }),
    }
}

fn override_decode_fail(
    ns: &str,
    key: &str,
    e: base64::DecodeError,
    snapshot_keys: u64,
) -> RunFail {
    RunFail {
        message: format!("decode {ns} override {key:?}: {e}"),
        logs: Vec::new(),
        snapshot_keys,
    }
}

fn cleanup_dry_run(storage: &Storage, dry_root: &str) -> Result<u64, StoreError> {
    let mut admin = storage.admin_bucket()?;
    admin.delete_with_prefix(dry_root)
}

fn into_dry_log(r: LogRecord) -> DryRunLog {
    DryRunLog {
        level: r.level.as_str().to_string(),
        message: r.message,
        timestamp: r.timestamp,
    }
}
