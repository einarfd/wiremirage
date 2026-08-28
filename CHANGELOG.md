# Changelog

Notable changes to WireMirage. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

WireMirage is pre-1.0: breaking changes are allowed on minor versions when
they make the design better, and are called out here.

## [Unreleased]

### Added

- **The route table revalidates on a match miss** (ADR-0037). When a request
  for a group that exists matches nothing, the dispatcher reloads that group's
  routes from storage and retries once — so a route created on one replica is
  reachable from another without a restart, which is the common agent workflow
  of creating a route and immediately sending traffic to it. Rate-limited to
  one reload per group per 5s, because unmatched traffic is precisely the
  traffic that misses and an unbounded read-through would let junk traffic
  amplify into a storage read per request. This makes `storage-model.md`'s
  cache-coherence guarantee true for the first time.
- **Cross-replica route-cache invalidation** (ADR-0037). Route creates,
  updates, deletes, group cascades and renames publish on a Valkey pub/sub
  channel; every replica subscribes and drops the affected records and compiled
  artifacts. This covers what the read-through cannot — a deleted or updated
  route still *matches*, so those requests never reach the miss path. Delivery
  is at-most-once by design: the records are already committed to storage
  before anything is published, so a lost message degrades to the
  read-through's staleness window instead of lasting until a restart. The
  subscriber reconnects with backoff, which includes the case of Valkey
  dropping a subscriber that hits the pubsub output-buffer limit.
  This adds the host's first *async* Valkey connection (`redis`'s
  `tokio-comp` feature); every request-path operation stays synchronous.
- **Cross-replica journal fan-out** (ADR-0037). Live tails — the SSE endpoint,
  the MCP `tail_journal` / `wait_for_request` tools, the live journal page and
  the group-detail pane — now see traffic dispatched by any replica instead of
  only the one holding the connection, which previously failed silently and
  partially: a tail saw roughly 1/N of matching requests and otherwise just
  kept waiting. Events publish to a per-group channel on the same connection
  the journal write already used, so the dispatch hot path gains no round trip.
  Replicas subscribe **lazily** — only while at least one local tail is
  attached — so a replica nobody is watching deserializes nothing. Existing
  subscribers are untouched; only the feed changed.
- **The login throttle is shared across replicas** (ADR-0037). Five failed
  password attempts in a minute lock an IP out host-wide instead of per
  replica, which previously multiplied the intended budget by the replica
  count. Implemented with deadline-carrying leases rather than `INCR` plus
  `EXPIRE`, so the in-memory backend — where `set_ttl` is a no-op — behaves
  identically instead of locking an IP out permanently.
- **One replica sweeps per tick** (ADR-0037). The lifecycle sweeper claims a
  short lease before each pass and the others skip it. Sweeping is idempotent,
  so this removes duplicated work rather than fixing a correctness problem, and
  an expiring lease means a replica dying mid-sweep can't wedge it.
- **User creation claims its email index atomically** (ADR-0037). The previous
  check-then-act could be passed by two replicas cold-starting against an empty
  store, or by two simultaneous first-logins for the same OIDC identity; the
  loser now gets the normal "email taken" error.

- **A Helm chart** at `deploy/helm/wiremirage`, now that multiple replicas are
  supported. Deployment, Service, Ingress and ConfigMap; storage and secrets
  come from outside the release. Values with no safe default — `apexHost`,
  `existingSecret`, `valkey.url`, `ingress.tls.secretName` — fail at render
  time with a message naming what to set, rather than deploying a host that
  would fail at boot. The ingress routes both the apex and `*.{apexHost}`,
  since groups get their own subdomains at runtime. `just helm-lint` (folded
  into `check-all`) lints it, renders it, and asserts a guardrail still fires.
  Pods run with a read-only root filesystem, a size-capped emptyDir for the
  compiled-engine cache, and a PodDisruptionBudget once there is more than one
  replica.
- **Settings page at `/ui/settings`** (admins only, linked from the nav). Full
  user management in the browser — list, create, promote, demote, delete —
  mirroring the `wm users` verbs and re-checking the same host guards, so the
  last admin can't be demoted or deleted and a user who still owns routes
  can't be removed. Closes the CLI+UI parity gap that left user
  administration CLI-only; MCP still deliberately excludes user management.
  The page also shows which identity providers are configured (read-only).
- **`POST /api/users/me/sessions/revoke-all`** — sign out everywhere, exposed
  as a button on the Settings page. Backed by a per-user session epoch rather
  than a session index: each session is stamped at creation and validation
  rejects anything behind the user's counter, so revoking is one increment and
  no session is enumerated. Returns 204 with no count for that reason. API
  tokens are a separate credential and are untouched. No CLI counterpart by
  design — sessions are a browser credential.

### Changed

- **The MCP transport is stateless** (ADR-0037). No `Mcp-Session-Id`
  is issued, `GET` and `DELETE` on `/api/mcp` return 405, and simple tools answer
  with plain JSON instead of an SSE frame. This is what lets consecutive MCP
  requests be served by different replicas. It is safe because the server sends
  no server-initiated messages — every tool, the two long-running streaming ones
  included, is a request that blocks and returns — and the protocol is removing
  sessions anyway in version 2026-07-28 (SEP-2567). Clients need no change: a
  server may decline to issue a session id, and the client's obligation to echo
  one is conditional on having been given one.

### Removed

- The `/ui/admin/health` placeholder page. The admin health screen and
  `GET /api/admin/health` are **not planned**: the OTLP spans and metric
  catalog cover the diagnostic case, and the shipped `/health` and `/ready`
  probes cover the operational one.
- Owner transfer (`PATCH /api/groups/{group}` with `owner_id`) and the
  `/api/admin/sessions` endpoints are likewise dropped from the design. Both
  were specified but never implemented; groups TTL out on their own and admins
  already have write access to any group, and session enumeration is more
  machinery than "sign out everywhere" needs.

## [0.1.0] — 2026-08-26

First public release.

### Handlers

- TypeScript and JavaScript handlers, transpiled in-host with swc and executed
  as WebAssembly components in wasmtime. No compile step, rebuild, or restart
  in the authoring loop.
- Two persistent key-value stores per handler: one private to the route, one
  shared across the group, with counters, lists, hashes, and sets.
- `host.sleep` for latency simulation, `host.responseStream` for Server-Sent
  Events and chunked responses, `host.scheduleCallback` for outbound webhooks.
- Outbound callbacks are the only egress from an otherwise-closed sandbox:
  off by default, opted into per host *and* per group, enforced on the
  resolved IP with special-use ranges denied.
- The handler contract ships as TypeScript in `types/wiremirage-handler.d.ts`,
  kept in step with `wit/wiremirage.wit` by a test that fails on either
  direction of drift.

### Routing and lifecycle

- Path patterns with named `{param}` segments and a trailing `{param...}`
  tail matcher, matched alongside the HTTP method (a comma-separated list, or
  `ANY`). Conflicting patterns are rejected at create time rather than
  resolved by declaration order.
- Groups are TTL-bounded lifecycle units that cascade-delete everything they
  own, and each group is a routing namespace served on its own subdomain.
- Every request is journaled with its envelope; requests that matched nothing
  land in a separate log with "did you mean…?" near-miss hints.

### Surfaces

- `wm` CLI with `--json` on every command, and shell completions.
- MCP server at `/api/mcp` with 33 tools, including live journal tailing and
  `wait_for_request`.
- Web UI at `/ui/` for inspecting and editing handlers, state, and journals.
- REST API at `/api/*`, which the CLI wraps 1:1.
- A skill at `skill/wiremirage/` that teaches an agent the workflow.

### Operations

- Storage on Valkey (Redis wire protocol) or in-memory.
- Bearer-token auth for the control plane; browser login via OIDC, GitHub
  OAuth, or local passwords. Mock traffic is never authenticated — systems
  under test don't carry credentials.
- OpenTelemetry traces and metrics over OTLP.
- Multi-arch container image on GHCR; static musl binaries for the CLI.
- Configuration fails fast at startup with a message naming what to set,
  rather than falling back silently.

[Unreleased]: https://github.com/einarfd/wiremirage/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/einarfd/wiremirage/releases/tag/v0.1.0
