//! Outbound-callback firing (ADR-0034).
//!
//! A handler schedules a callback via `host.scheduleCallback`; the host buffers
//! it (see [`crate::host_state`]) and, after the response is sent, hands the
//! buffered callbacks here. Each fires ONCE on its own background task after the
//! requested delay, and the outcome is recorded in the journal — it can't ride
//! the original response, which already returned.
//!
//! The security-critical part is the egress check: we resolve the target host
//! ourselves, check **every** resolved address against [`EgressPolicy`]
//! (deny-if-any), and pin reqwest's DNS to exactly those vetted addresses so a
//! second resolution can't rebind to a blocked IP between check and connect.
//! Redirects are disabled (a 3xx is recorded as-is, never followed).
//!
//! Single-attempt, best-effort: no retries, no durable queue. A callback in
//! flight is lost on host restart (ADR-0034 — a deliberate non-goal).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::egress::{EgressDecision, EgressPolicy};
use crate::journal::{CallbackOutcome, Journal, NewCallbackEntry, truncate_body};

/// Hard cap on the requested delay. A handler can't schedule a callback further
/// out than this — it would just be dropped on the next restart anyway, and an
/// unbounded delay would pin a background task indefinitely.
const MAX_DELAY_MS: u64 = 300_000;

/// Per-callback wall-clock budget for the whole fire (DNS + connect + request).
const FIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the request body stored on the callback journal record.
const REQUEST_BODY_JOURNAL_LIMIT: usize = 16 * 1024;

/// A callback a handler asked the host to send, captured during `handle` and
/// fired after the response. Mirrors the `callback.schedule` WIT args.
#[derive(Debug, Clone)]
pub struct ScheduledCallback {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay_ms: u64,
}

/// Journal attribution for the callbacks a single request scheduled.
#[derive(Debug, Clone)]
pub struct CallbackContext {
    pub trace_id: Option<String>,
    pub group_id: String,
    pub group_name: String,
    pub route_id: String,
    pub route_number: u32,
}

/// Spawn one background task per scheduled callback. Returns immediately; each
/// task sleeps its delay, fires once through the egress filter, and journals
/// the outcome. The `egress`/`journal` handles are cheap clones.
pub fn spawn_callbacks(
    journal: Journal,
    egress: Arc<EgressPolicy>,
    ctx: CallbackContext,
    callbacks: Vec<ScheduledCallback>,
) {
    for cb in callbacks {
        let journal = journal.clone();
        let egress = egress.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            fire_one(journal, egress, ctx, cb).await;
        });
    }
}

async fn fire_one(
    journal: Journal,
    egress: Arc<EgressPolicy>,
    ctx: CallbackContext,
    cb: ScheduledCallback,
) {
    let delay = cb.delay_ms.min(MAX_DELAY_MS);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }

    let started = Instant::now();
    let outcome = deliver(&egress, &cb).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    let (body, body_size, body_truncated) =
        truncate_body(cb.body.clone(), REQUEST_BODY_JOURNAL_LIMIT);
    let entry = NewCallbackEntry {
        trace_id: ctx.trace_id,
        group_id: ctx.group_id,
        group_name: ctx.group_name,
        route_id: ctx.route_id,
        route_number: ctx.route_number,
        url: cb.url,
        method: cb.method,
        request_headers: cb.headers,
        request_body: body,
        request_body_truncated: body_truncated,
        request_body_size: body_size,
        delay_ms: cb.delay_ms,
        outcome,
        duration_ms,
    };
    if let Err(e) = journal.record_callback(entry) {
        tracing::warn!(error = %e, "failed to record callback journal entry");
    }
}

/// Resolve, egress-check, and fire a single callback. Never panics; every
/// failure maps to a [`CallbackOutcome`].
async fn deliver(egress: &EgressPolicy, cb: &ScheduledCallback) -> CallbackOutcome {
    // Parse + validate the URL up front.
    let url = match reqwest::Url::parse(&cb.url) {
        Ok(u) => u,
        Err(e) => {
            return CallbackOutcome::Failed {
                error: format!("invalid url: {e}"),
            };
        }
    };
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return CallbackOutcome::Failed {
                error: format!("unsupported url scheme {other:?} (only http/https)"),
            };
        }
    }
    let Some(host) = url.host_str().map(str::to_owned) else {
        return CallbackOutcome::Failed {
            error: "url has no host".into(),
        };
    };
    let Some(port) = url.port_or_known_default() else {
        return CallbackOutcome::Failed {
            error: "url has no port".into(),
        };
    };

    let method = match reqwest::Method::from_bytes(cb.method.to_ascii_uppercase().as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            return CallbackOutcome::Failed {
                error: format!("invalid method {:?}", cb.method),
            };
        }
    };

    // Resolve the host ourselves so the egress decision is made on the actual
    // addresses we will connect to — never the hostname string.
    let socket_addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(iter) => iter.collect(),
        Err(e) => {
            return CallbackOutcome::Failed {
                error: format!("dns resolution failed: {e}"),
            };
        }
    };
    let ips: Vec<std::net::IpAddr> = socket_addrs.iter().map(|sa| sa.ip()).collect();
    if let EgressDecision::Deny(reason) = egress.check_resolved(&ips) {
        return CallbackOutcome::EgressDenied {
            reason: reason.to_string(),
            resolved: ips.iter().map(|ip| ip.to_string()).collect(),
        };
    }

    // Pin reqwest's DNS to exactly the vetted addresses: no second resolution,
    // so a rebinding response can't redirect us to a blocked IP. Redirects off
    // and a hard timeout bound the single attempt.
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FIRE_TIMEOUT)
        .resolve_to_addrs(&host, &socket_addrs)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CallbackOutcome::Failed {
                error: format!("client build failed: {e}"),
            };
        }
    };

    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &cb.headers {
        // Host + Content-Length are managed by reqwest; skip handler-set copies.
        let lname = name.to_ascii_lowercase();
        if lname == "host" || lname == "content-length" {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            headers.append(hn, hv);
        }
    }

    match client
        .request(method, url)
        .headers(headers)
        .body(cb.body.clone())
        .send()
        .await
    {
        Ok(resp) => CallbackOutcome::Delivered {
            status: resp.status().as_u16(),
        },
        Err(e) => CallbackOutcome::Failed {
            error: format!("request failed: {e}"),
        },
    }
}
