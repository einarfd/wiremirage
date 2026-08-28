//! Cross-replica message bus over Valkey pub/sub (ADR-0037).
//!
//! The host's caches are per-process. A replica that serves an API call
//! updates its own route table through the `refresh_*` hooks, but its
//! siblings learn nothing. [`RouteTable::revalidate_and_rematch`] gives
//! creates a floor that needs no messaging — a miss reloads the group —
//! but a *deleted* or *updated* route still matches, so those requests
//! never reach the miss path. This module is what makes them timely.
//!
//! Two properties are deliberate:
//!
//! **At-most-once is the right semantic.** The bus carries liveness, not
//! durability — the route records themselves are already committed to
//! storage before anything is published, and the read-through floor
//! bounds what a lost message costs. A dropped invalidation degrades to
//! the pre-existing staleness window, now capped by the revalidation
//! interval rather than by process lifetime.
//!
//! **Reconnection is the actual work.** Nothing else in the codebase has
//! to survive a dropped connection: the sync store opens a connection
//! per operation, so a failure surfaces as one failed request. A
//! subscriber holds its connection open for the process lifetime, so it
//! must expect to lose it — including Valkey disconnecting a subscriber
//! that falls far enough behind to hit the pubsub output-buffer limit.
//! That is expected, not exceptional, and the loop below treats it so.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Storage;
use crate::route_table::RouteTable;

/// Channel carrying route-cache invalidations. One host-wide channel:
/// the volume is control-plane mutations, which are rare next to mock
/// traffic, and every replica needs every event.
pub const ROUTE_INVALIDATION_CHANNEL: &str = "wm:invalidate:routes";

/// Backoff bounds for the subscriber's reconnect loop.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(250);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A route-cache invalidation. Carries the group whose route set
/// changed plus the specific routes whose compiled artifacts must be
/// dropped.
///
/// The group id alone would be enough to reload records, but not to
/// evict the compiled-component and transpiled-JS caches — those are
/// keyed by route id, and an update that changes a handler's source
/// without changing its path would otherwise keep serving stale bytes
/// from a cache the reload never touches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteInvalidation {
    pub group_id: String,
    /// Routes whose compiled artifacts are now stale. Empty is valid —
    /// a pure create adds a record without invalidating any artifact.
    #[serde(default)]
    pub route_ids: Vec<String>,
}

impl RouteInvalidation {
    pub fn for_group(group_id: impl Into<String>) -> Self {
        Self {
            group_id: group_id.into(),
            route_ids: Vec::new(),
        }
    }

    pub fn with_routes(mut self, route_ids: Vec<String>) -> Self {
        self.route_ids = route_ids;
        self
    }
}

/// Publish an invalidation, best-effort.
///
/// Failures are logged and swallowed on purpose. The mutation that
/// triggered this has already been committed to storage and applied
/// locally; refusing the API call because siblings could not be told
/// would turn a degraded-timeliness problem into a failed write, and
/// the read-through floor already bounds the cost of a missed message.
pub fn publish_route_invalidation(storage: &Storage, event: &RouteInvalidation) {
    if !storage.is_distributed() {
        return;
    }
    let payload = match serde_json::to_vec(event) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "encoding route invalidation");
            return;
        }
    };
    let mut bucket = match storage.admin_bucket() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "opening bucket to publish route invalidation");
            return;
        }
    };
    match bucket.publish(ROUTE_INVALIDATION_CHANNEL, payload) {
        Ok(n) => tracing::debug!(
            group_id = %event.group_id,
            routes = event.route_ids.len(),
            receivers = n,
            "published route invalidation"
        ),
        Err(e) => tracing::warn!(error = %e, "publishing route invalidation"),
    }
}

/// A running route-invalidation subscriber.
///
/// Dropping this aborts nothing — the task is detached and dies with the
/// process, like the sweeper and the epoch ticker.
pub struct InvalidationSubscriber {
    pub handle: tokio::task::JoinHandle<()>,
    ready: tokio::sync::watch::Receiver<bool>,
}

impl InvalidationSubscriber {
    /// Resolve once the subscription has been established at least
    /// once. Pub/sub is at-most-once with no replay, so anything
    /// published before the first successful subscribe is simply not
    /// heard — tests that publish and then assert must await this
    /// rather than sleeping and hoping.
    pub async fn wait_ready(&mut self) {
        if *self.ready.borrow() {
            return;
        }
        let _ = self.ready.changed().await;
    }
}

/// Spawn the route-invalidation subscriber.
///
/// Returns `None` on the in-memory backend, where there are no siblings
/// and the local `refresh_*` hooks are already authoritative.
pub fn spawn_route_invalidation_subscriber(
    storage: Storage,
    routes: Arc<RouteTable>,
) -> Option<InvalidationSubscriber> {
    if !storage.is_distributed() {
        tracing::debug!("in-memory storage: no route-invalidation subscriber needed");
        return None;
    }
    let (ready_tx, ready) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        let mut backoff = RECONNECT_BACKOFF_START;
        loop {
            match run_route_invalidation_session(&storage, &routes, &ready_tx).await {
                Ok(()) => {
                    // The stream ended without an error — the server
                    // closed it or the connection went away quietly.
                    // Same handling as an error: reconnect.
                    tracing::info!("route-invalidation subscription ended; reconnecting");
                    // A clean session means the connection worked, so
                    // don't carry a punitive backoff into the retry.
                    backoff = RECONNECT_BACKOFF_START;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "route-invalidation subscription failed");
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                }
            }
            tokio::time::sleep(backoff).await;
        }
    });
    Some(InvalidationSubscriber { handle, ready })
}

/// One connect-subscribe-consume cycle. Returns when the stream ends;
/// the caller reconnects.
async fn run_route_invalidation_session(
    storage: &Storage,
    routes: &Arc<RouteTable>,
    ready: &tokio::sync::watch::Sender<bool>,
) -> Result<(), String> {
    use futures::StreamExt as _;

    let mut pubsub = storage
        .pubsub()
        .await
        .ok_or_else(|| "storage is not distributed".to_string())?
        .map_err(|e| format!("{e}"))?;
    pubsub
        .subscribe(ROUTE_INVALIDATION_CHANNEL)
        .await
        .map_err(|e| format!("subscribe: {e}"))?;
    tracing::info!(
        channel = ROUTE_INVALIDATION_CHANNEL,
        "route-invalidation subscriber connected"
    );
    // Resync on every successful subscribe, first one included.
    //
    // Pub/sub has no replay, so anything published while this replica
    // was disconnected is gone — and deletes and updates are exactly
    // what the read-through floor cannot recover, since a stale route
    // still matches and never reaches the miss path. Without this a
    // five-second blip could leave a route serving forever, and the
    // claim that a lost message degrades to the revalidation window
    // would be false.
    //
    // Unconditional rather than only-on-reconnect: it costs one
    // redundant reload at startup, just after `warm`, and avoids a
    // "was this the first connection?" flag that would be wrong exactly
    // once, in the case that matters.
    routes.reload_all();
    let _ = ready.send(true);

    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: Vec<u8> = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "unreadable invalidation payload");
                continue;
            }
        };
        let event: RouteInvalidation = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(e) => {
                // A malformed message is one bad message, not a reason
                // to tear down a healthy subscription.
                tracing::warn!(error = %e, "undecodable invalidation payload");
                continue;
            }
        };
        // The originating replica hears its own message back and
        // re-applies it. That is harmless — applying an invalidation is
        // idempotent — and keeps one delivery path rather than two.
        routes.apply_invalidation(&event.group_id, &event.route_ids);
    }
    Ok(())
}

// -- Journal fan-out (ADR-0037 item 2) --------------------------------------
//
// The journal's live tail — the SSE endpoint, the two MCP streaming tools,
// the live journal page and the group-detail live pane — reads a local
// broadcast channel. Under more than one replica that means a tail sees only
// the traffic its own replica happened to dispatch: roughly 1/N of matching
// requests, with the rest never arriving. The failure is silent and partial,
// which is worse than a loud one — an agent waiting on a request just keeps
// waiting.
//
// So the dispatching replica publishes to its group's channel and every
// replica holding a tail pulls events back into its own local broadcast.
// Existing subscribers are untouched; only the feed changes.

/// How often an active subscription re-checks whether anyone is still
/// tailing. Dropping the subscription late costs a little deserialization;
/// it is a cost optimization, not a correctness property, so a coarse
/// check is fine.
const JOURNAL_IDLE_CHECK: Duration = Duration::from_secs(5);

/// A running journal fan-out subscriber.
pub struct JournalSubscriber {
    pub handle: tokio::task::JoinHandle<()>,
    ready: tokio::sync::watch::Receiver<bool>,
}

impl JournalSubscriber {
    /// Resolve once the subscription is established. Pub/sub has no
    /// replay, so a test that publishes before this resolves would be
    /// asserting on events that were never delivered.
    pub async fn wait_ready(&mut self) {
        if *self.ready.borrow() {
            return;
        }
        let _ = self.ready.changed().await;
    }
}

/// Spawn the journal fan-out subscriber.
///
/// **Subscribes lazily.** A replica connects only while it holds at least
/// one local tail, and drops the subscription when the last one leaves.
/// Without this, every replica would deserialize every event for every
/// group whether or not anyone was watching — the one part of this design
/// that scales with traffic times replicas. With it, the fan-out costs
/// nothing in the common case (nobody tailing) and is paid only for the
/// duration of an actual tail.
///
/// Returns `None` on the in-memory backend, where the journal feeds its
/// local broadcast directly.
pub fn spawn_journal_subscriber(journal: crate::journal::Journal) -> Option<JournalSubscriber> {
    if !journal.storage().is_distributed() {
        tracing::debug!("in-memory storage: journal fan-out is local");
        return None;
    }
    let (ready_tx, ready) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        let mut backoff = RECONNECT_BACKOFF_START;
        loop {
            // Idle until something is actually tailing here.
            journal.await_demand().await;
            let outcome = run_journal_session(&journal, &ready_tx).await;
            let _ = ready_tx.send(false);
            match outcome {
                // Nobody is tailing: loop straight back to awaiting
                // demand, with no sleep and no backoff to carry.
                Ok(SessionEnd::NoDemand) => {
                    backoff = RECONNECT_BACKOFF_START;
                    continue;
                }
                // The stream died while tails were still attached. Fall
                // through to the sleep — otherwise `await_demand`
                // returns immediately and this spins.
                Ok(SessionEnd::StreamEnded) => {
                    tracing::info!("journal fan-out stream ended; reconnecting");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "journal fan-out subscription failed");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
        }
    });
    Some(JournalSubscriber { handle, ready })
}

/// Why a fan-out session ended. The distinction matters: idling because
/// nobody is tailing must not be treated like a connection that died,
/// or a Valkey accepting and immediately dropping subscribers would spin
/// with no backoff.
enum SessionEnd {
    /// The last local tail went away; drop the subscription and idle.
    NoDemand,
    /// The stream ended under us; reconnect with backoff.
    StreamEnded,
}

/// One subscribe-and-pump cycle.
async fn run_journal_session(
    journal: &crate::journal::Journal,
    ready: &tokio::sync::watch::Sender<bool>,
) -> Result<SessionEnd, String> {
    use futures::StreamExt as _;

    let mut pubsub = journal
        .storage()
        .pubsub()
        .await
        .ok_or_else(|| "storage is not distributed".to_string())?
        .map_err(|e| format!("{e}"))?;
    // A pattern subscription covers every group. Per-group channels are
    // still what publishers use, so narrowing this to only the groups a
    // replica is actually tailing stays a purely subscriber-side change
    // — no wire-format break — if the deserialization cost ever shows up.
    pubsub
        .psubscribe(crate::journal::JOURNAL_CHANNEL_PATTERN)
        .await
        .map_err(|e| format!("psubscribe: {e}"))?;
    tracing::info!(
        pattern = crate::journal::JOURNAL_CHANNEL_PATTERN,
        "journal fan-out subscriber connected"
    );
    let _ = ready.send(true);

    let mut stream = pubsub.on_message();
    loop {
        let next = tokio::time::timeout(JOURNAL_IDLE_CHECK, stream.next()).await;
        match next {
            // Timed out waiting for an event: check whether anyone is
            // still listening, and let go of the subscription if not.
            Err(_) => {
                if journal.local_subscriber_count() == 0 {
                    tracing::debug!("last journal tail left; dropping the subscription");
                    return Ok(SessionEnd::NoDemand);
                }
            }
            Ok(None) => return Ok(SessionEnd::StreamEnded),
            Ok(Some(msg)) => {
                let payload: Vec<u8> = match msg.get_payload() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "unreadable journal event payload");
                        continue;
                    }
                };
                match serde_json::from_slice::<crate::journal::PublishedJournalEvent>(&payload) {
                    // Our own event, already delivered locally by the
                    // publish path. Dropping it here is what lets the
                    // origin deliver synchronously without seeing every
                    // event twice.
                    Ok(wire) if wire.origin == journal.origin() => {}
                    Ok(wire) => journal.deliver_local(wire.event),
                    Err(e) => {
                        tracing::warn!(error = %e, "undecodable journal event payload");
                    }
                }
            }
        }
    }
}
