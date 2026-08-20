# ADR-0037: Multi-replica readiness — the per-process state blocking replicas > 1

**Status:** Proposed

## Context

Valkey was chosen as the storage backend ([0005-valkey-storage.md](0005-valkey-storage.md), [0014-valkey-not-redis.md](0014-valkey-not-redis.md)) partly so host state lives outside the process and the host can scale horizontally. ../architecture-overview.md records the current position honestly: multi-host scale-out is untested, and "conceptually possible" because the in-memory cache is best-effort with a Valkey-backed fallback.

The operator now wants to run WireMirage as several containers behind a Helm chart. That turns "conceptually possible" into a requirement, so the host was audited for state that still lives in the process.

The audit found six items: three break correctness under more than one replica, three degrade behaviour without breaking it. It also found that the fallback the design docs lean on does not exist.

### The correctness breaks

**1. The route table is a process-local snapshot with no cross-replica invalidation and no read-through.** The route table holds its routes in an in-process vector, warmed once at startup and mutated only by the local refresh hooks invoked by whichever replica served the API call. Route *matching* iterates that vector directly. There is no path from a match miss back to Valkey.

Under more than one replica:

- **Create on A is invisible on B** until B restarts. B returns its no-match response and writes an unmatched journal entry. This is the *common* agent workflow — create a route, immediately send traffic to it — so with N replicas it fails on roughly (N-1)/N of requests.
- **Delete on A leaves B serving the route.**
- **A source update on A leaves B serving the old source**, because the compiled-component and transpiled-JS caches are keyed by route id and evicted only by the local hook.

This is the significant finding, because ../storage-model.md "Cache coherence and route readiness" states the opposite. It guarantees that a successful route creation means the route is reachable from any host backed by the same Valkey, on the strength of a cache-miss read-through. Its own "Implementation status" subsection anticipates the delete case and asserts the read-through still covers correctness. It does not. The lazy populate-on-miss exists only for the compiled-artifact caches, and those are reached *after* a route record has already been found in the process-local vector — so they can never rescue a record the vector has never seen. The create case, which the doc treats as safe, is the one that breaks worst.

**2. The journal live-tail bus is in-process.** The journal fans out over a local broadcast channel. Everything live — the two MCP streaming tools, the SSE tail endpoint, the live journal page and the group-detail live pane — observes only traffic dispatched by the replica holding the connection. Stored journal reads are unaffected because they go to Valkey, so the failure is silent and partial: an agent waiting on a request sees roughly 1/N of matching traffic and otherwise keeps waiting.

**3. MCP streamable-HTTP sessions are process-local.** The service is built with rmcp's local session manager. A client's follow-up request routed to a different replica finds no session.

### The degradations

**4. Login throttle.** Per-IP counters live in an in-process map, so N replicas allow N times the intended failed-login budget before lockout.

**5. Lifecycle sweeper.** Runs on every replica. Every operation it performs is idempotent — its own module docs reason about racing sweepers converging harmlessly — so this is duplicated scan work rather than a correctness problem. Those docs also already flag the cross-host cache-invalidation gap that item 1 covers.

**6. Bootstrap admin creation.** The bootstrap path is check-then-act, so replicas cold-starting simultaneously against an empty Valkey can race.

### Already correct

Browser sessions, the OAuth/OIDC nonce and PKCE verifier, tokens, users, route and group records, handler state, dry-run snapshot namespaces and the journal itself all live in Valkey. CSRF is a stateless double-submit cookie. The wasmtime engine, the epoch ticker, the embedded engine bytes and the transpile cache are per-process by design and correct that way.

Outbound callbacks ([0034-outbound-callbacks.md](0034-outbound-callbacks.md)) fire on the replica that served the request; if it terminates during the delay, the callback is lost. That sits inside the existing single-attempt best-effort contract, so no change is needed — but rolling deploys make it likelier, and it should be documented rather than discovered.

## Decision

Close the gaps as one slice, on one new mechanism — **an application-level pub/sub bus over Valkey** — plus **a read-through correctness floor for route matching that does not depend on message delivery**.

### Cache coherence (item 1): read-through first, invalidation second

- On a match miss for a group that exists, the dispatcher revalidates that group's routes from Valkey and retries the match once, rate-limited to at most one revalidation per group per interval (default 5s). This makes the documented readiness guarantee true: a committed route creation is reachable from any replica with no dependence on messaging. The rate limit is load-bearing — unmatched traffic is precisely the traffic that misses, so an unbounded read-through turns junk traffic into a Valkey amplifier.
- Route create, update and delete publish an invalidation event. Subscribers reload or drop the affected route and evict its compiled-component and transpiled-JS entries. This is what makes delete and update *timely*. A lost message degrades to the pre-existing staleness window, now bounded by the revalidation interval instead of by process lifetime.

### Journal fan-out (item 2): publish to Valkey, deliver locally

- The journal's publish path branches on the backend: in-memory sends to the local broadcast as today; Valkey publishes to a per-group channel and does nothing locally.
- One subscriber task per replica pattern-subscribes, deserializes, and re-sends into the local broadcast. Every existing subscriber is untouched — only the feed changes.
- The originating replica receives its own event back through Valkey. This is deliberate: one uniform delivery path. Publishing both locally and to Valkey would double-deliver on the origin.
- Per-group channels, so a group-scoped tail does not deserialize every other group's traffic; the admin host-wide tail is the pattern subscription.
- The payload is the serialized record rather than an id to re-fetch, because journal bodies are already capped at 16 KiB.
- **Subscribe lazily.** A replica subscribes only while it holds at least one local tail subscriber, and drops the subscription when the last one leaves. Without this, every replica deserializes every event for every group whether or not anyone is watching — the one part of this design that scales with traffic times replicas. With it, the fan-out cost is zero in the common case (nobody tailing) and paid only for the duration of an actual tail.
- **Publish is pipelined with the journal write, not added to it.** The buffered dispatch path already performs its journal write inline, before the response returns; issuing the publish as a second round trip would add latency to every mock request. Both commands go to the same Valkey on the same connection, so they pipeline into one round trip and the marginal cost is approximately nil.

Pub/sub is the right primitive here rather than Streams. The journal is already persisted in Valkey under its own TTL, so the bus carries liveness, not durability — and the existing in-process bus is already lossy by design, a bounded broadcast that surfaces lag to its callers. At-most-once delivery is a semantic match for what is being replaced.

### The rest

- **MCP sessions (item 3): turn sessions off.** The transport is switched to stateless — the never-session manager plus legacy session mode disabled — rather than made shared or pinned. This is viable because the MCP server has *no* server-initiated messages: every tool, including the two long-running streaming tools, is a plain request that returns a result. Nothing needs a channel the server can push down between requests. The protocol is heading the same way on its own (SEP-2567 removes sessions as of protocol version 2026-07-28, and rmcp already serves clients negotiating that version statelessly regardless of the setting), so this is adopting the destination early rather than working around a constraint. Legacy clients lose only SSE reconnection priming, which a request-response tool set does not use. Preferring plain JSON responses for simple tools is the natural companion setting.
- **Login throttle (item 4): move the counters to Valkey.** An increment plus an expiry on a per-IP key is the canonical distributed rate limit, and is less work than reasoning about per-replica budgets.
- **Sweeper (item 5): one sweeper at a time**, via a lock key acquired with a TTL before each pass; non-holders skip. Idempotency already makes this optional, so it is a cost reduction rather than a correctness fix.
- **Bootstrap (item 6): a set-if-not-exists guard** so simultaneous cold starts converge on one record.
- **Callbacks (item 7): no change**, plus an explicit note in ADR-0034's consequences and in the chart docs that a replica terminating mid-delay drops that callback.

### Non-goals

- Valkey keyspace notifications, which storage-model.md proposed. See alternatives.
- Any leader-election framework. The one place that wants leadership is served by a lock key with a TTL.
- A shared compiled-component cache. Compilation is per-process and cheap next to a network hop.
- Making the host stateless. The route table stays a cache; the decision is that it becomes a *coherent* one.

## Consequences

- More than one replica becomes supportable, which is the prerequisite for the Helm chart. Until this lands the chart should pin a single replica, and that pin is the honest default to ship with.
- The host gains its first *async* Valkey connection. The store layer is entirely synchronous today — a connection per operation — so this introduces an async connection type, a subscriber task, and, the actual work in the slice, a reconnect-with-backoff loop. Nothing in the codebase currently has to survive a dropped connection.
- Two design docs are wrong today and are corrected in the same change: storage-model.md's "Cache coherence and route readiness" (the read-through it describes does not exist; this ADR makes it exist) and architecture-overview.md's multi-host paragraph, which repeats the claim.
- The read-through adds a Valkey round trip on match misses, which is to say on unmatched traffic. The per-group rate limit bounds it: one group reload per group per interval regardless of junk volume.
- Ordering is per-channel, not global. Journal records carry a per-group sequence number assigned by Valkey, so consumers can order and de-duplicate on it.
- A subscriber that falls far behind will hit Valkey's pubsub output-buffer limit and be disconnected. At 16 KiB per message that is a long way out, but the reconnect loop must treat it as expected rather than exceptional.
- Single-replica deployments see no behavioural change beyond the read-through, which is a strict improvement, and the throttle moving to Valkey.

### MCP client compatibility

Stateless mode changes three things on the wire, and the Streamable HTTP specification makes all three server-optional with precisely the responses rmcp gives:

- **No session id is issued.** A server "**MAY** assign a session ID"; the client obligation to echo it is conditional — "**If** an `Mcp-Session-Id` is returned by the server during initialization, clients ... **MUST** include it". No id returned means no id expected.
- **GET returns 405.** The client "**MAY** issue an HTTP GET" to open a server-to-client stream, and the server "**MUST** either return `Content-Type: text/event-stream` ... **or else return HTTP 405 Method Not Allowed**, indicating that the server does not offer an SSE stream at this endpoint."
- **DELETE returns 405.** Explicitly permitted: the server "**MAY** respond to this request with HTTP 405 Method Not Allowed, indicating that the server does not allow clients to terminate sessions."

POST — the path every tool call takes — is unaffected, and a client "**MUST** support both" a JSON and an SSE response to it, so preferring JSON for simple tools is equally safe. Clients negotiating an older protocol version still work: with legacy session mode off, their requests are routed down the stateless path rather than rejected.

The functional capability actually given up is server-initiated messages outside a client request. WireMirage sends none — there are no notifications, progress, sampling or elicitation calls anywhere in the MCP module, and the two long-running streaming tools are ordinary request-response calls that block and return. So the capability is unused, not merely tolerable to lose.

A spec-compliant client therefore cannot break on this. That is an argument about conformance rather than about any given client's tolerance, so it was also verified empirically against a real client.

**Verified (2026-08-13).** A host built with the stateless configuration was exercised end to end:

- `GET` on the MCP endpoint returned `405` with `Allow: POST`, as the specification requires of a server not offering a stream there.
- `initialize` returned `200` with no session-id header and a plain JSON body, negotiating protocol version 2025-06-18.
- `tools/list` (33 tools) and a `tools/call` both succeeded as **independent requests carrying no session id** — which is exactly the multi-replica case, where consecutive requests may land on different replicas.
- Claude Code's own MCP client, pointed at the endpoint, reported the server as connected.

The verification did not cover Codex, whose client was unavailable in the test environment. Given the conformance argument above and that both vendors track the same specification, the residual risk is judged low; a smoke test there before rollout is still the cheap precaution. Reverting is in any case a flag flip — this is deployment configuration, not a change to any persisted or published format.

### Performance

The number that matters for a mock server is matched-request latency, and it does not change.

- **Matched mock traffic (the hot path): no added latency.** Matching remains an in-process read over the cached routes. The only addition is the journal publish, and because the buffered dispatch path already writes the journal inline before responding, the publish pipelines into that existing round trip rather than adding one. When nobody is tailing, lazy subscribe means there is no publish at all.
- **Unmatched mock traffic: one group reload per group per interval.** A miss triggers a reload of that group's routes — a set read plus a hash read per route, pipelined, single-digit milliseconds against a same-network Valkey for a group of ordinary size. The rate limit is what bounds this: under a junk-traffic flood the cost is per group per interval, not per request, so an attacker or a broken client cannot amplify it.
- **Live tail: paid only while someone is tailing.** Each subscribed replica deserializes the events its subscription covers. This is the only cost that scales with traffic times replicas, which is exactly why the subscription is lazy.
- **Control plane: one publish per route mutation**, against an operation that already performs several writes.
- **Login throttle: one increment plus an expiry per attempt.** Sweeper: one lock attempt per replica per sweep interval. Bootstrap: once per process start. All negligible.

The costs that do exist are therefore concentrated on unmatched traffic and on active tails — both exceptional paths — rather than on the matched request that represents the product's actual work.

## Alternatives considered

- **Valkey keyspace notifications**, as proposed in storage-model.md. Rejected as the primary mechanism: they require server configuration that is off by default and may not be settable on a managed Valkey; they couple cache invalidation to physical key naming rather than to an application event we control and can version; and they are themselves pub/sub, carrying the same at-most-once loss without being more reliable. An application-level channel costs the same machinery and is explicit. The read-through floor is what actually buys correctness in either design.
- **Valkey Streams for the journal bus.** Rejected: consumer groups solve the wrong problem — work distribution, not fan-out — and plain stream reads with per-subscriber cursors plus length trimming is more machinery for the same delivery, when durability is already handled by the persisted journal. Reconsider if replay-on-reconnect becomes a requirement.
- **Drop the route cache and read from Valkey per mock request.** Rejected on latency: matching needs every route in the group, so this is a scan plus a multi-key read on every mock request, against a cache hit rate that is otherwise effectively total.
- **Sticky sessions at the ingress for MCP** (the first draft of this decision), or sticky sessions generally. Rejected once it was established that the MCP server has no server-initiated messages: affinity would constrain the ingress and add a deployment requirement to work around session state that does not need to exist. Applied generally it is worse still, because it papers over item 1 rather than fixing it — a route created through one replica would remain invisible to mock traffic pinned to another.
- **A Valkey-backed session manager for MCP.** Rejected: third-party trait work to distribute state that the stateless transport removes outright.
- **Defer the whole thing and ship the chart at one replica.** A legitimate position, and this ADR does not block it: the chart is useful single-replica and none of this is urgent until horizontal scale is actually wanted. Recorded as the sequencing decision rather than a rejected alternative — pin the replica count, land this slice when scale-out matters.

## See also

- ../storage-model.md — "Cache coherence and route readiness", corrected by this ADR
- ../architecture-overview.md — deployment shape and the multi-host note this ADR supersedes
- [0005-valkey-storage.md](0005-valkey-storage.md), [0014-valkey-not-redis.md](0014-valkey-not-redis.md) — why the shared store is Valkey
- [0013-groups-first-class.md](0013-groups-first-class.md) — the group lifecycle the sweeper serves
- [0024-metrics-via-otlp.md](0024-metrics-via-otlp.md) — the observability surface that makes stale-cache and fan-out gaps visible
- [0034-outbound-callbacks.md](0034-outbound-callbacks.md) — the callback-loss note this ADR adds to its consequences
