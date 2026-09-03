# Changelog

Notable changes to WireMirage. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

WireMirage is pre-1.0: breaking changes are allowed on minor versions when
they make the design better, and are called out here.

## [Unreleased]

### Changed

- **The web UI now has two clearly separated areas: your account and host
  administration.** Following the sign-out-everywhere bug below, the UI was
  reorganised around the distinction that bug turned on — credentials you hold
  versus the host you administer.

  - **`/ui/me/tokens` → `/ui/me`, "Your account".** One screen for all three
    credentials a user holds: API tokens, authorised MCP applications, and
    browser sessions. The POST action routes keep their `/ui/me/tokens/...`
    paths; only the page moved.
  - **`/ui/settings` → `/ui/admin`, "Admin".** "Settings" named a topic where
    the screen enforces a privilege, and the vagueness was load-bearing — it is
    why a user looking for account settings lands on a user-administration
    table. Users and identity providers are all that remain there.
  - **The admin gate is now structural** (ADR-0039). Every route under
    `/ui/admin` sits behind one `require_admin` router layer instead of four
    per-handler `is_admin` checks, so the path prefix and the privilege cannot
    disagree. A sweep test asserts every admin route refuses a non-admin;
    axum exposes no route table, so that list is hand-maintained and is a
    backstop rather than a proof.
  - **The header user area is one identity chip** linking to your account:
    initials, email, and the `admin` role when you have it. The email stays
    visible (on a multi-user host, "which account am I on?" is a real
    question), initials are derived locally rather than fetched from gravatar,
    and admin is spelled out rather than signalled by colour alone.
  - Home's quick actions gained Unmatched, your account, and — for admins —
    Admin. The nav bar wraps instead of overflowing on narrow viewports.

  Old URLs are not redirected. Pre-1.0, and the moved paths are a page and its
  form targets, not an API.

### Added

- **The web UI reports the running host version**, in a footer on every
  authenticated screen. This closes a cross-surface gap rather than adding a
  feature: the version was already on `GET /health`, on `wm version`, and in
  `summarize_workspace`'s `host` block, and the UI was the only surface that
  could not answer "which build am I looking at?". It is not admin-gated —
  `/health` serves it unauthenticated, so gating it would claim a privilege
  the data doesn't have.

  Note the limit: between releases every build off `main` reports the version
  of the last bump, so two images can both say `0.1.2`. Identifying an exact
  build still means checking the image digest.

### Fixed

- **Non-admins could not sign out everywhere from the browser.** "Sign out
  everywhere" is self-service — it bumps the caller's own session epoch and can
  only ever affect the caller, which is why the handler has never been
  admin-gated. But its only affordance was a button on `/ui/settings`, which
  403s for non-admins, so a non-admin who wanted to end their sessions had no
  UI path to the action at all. They would have needed to hand-roll the REST
  call with an API token — no help if the credential they are worried about is
  a stolen browser session.

  The action moves to the namespace that matches its blast radius:
  `POST /ui/settings/sessions/revoke-all` → `POST /ui/me/sessions/revoke-all`,
  rendered as a "Sessions" card on the tokens page, which is nav-linked for
  every user and already the home of the other two credentials (API tokens and
  MCP grants). Removed from the Settings page, which is now purely host
  administration. Behaviour is unchanged; only the location and the URL move.

  The old URL is gone rather than redirected — pre-1.0, and a browser form
  target is not a surface anyone has bookmarked.

  Also fixes a latent trap: a self-service route sat inside `/ui/settings/*`,
  whose every other member is admin-only. Anyone adding a blanket admin layer
  to that subtree — the obvious tidy-up — would have silently deleted a
  security control that users rely on.

## [0.1.2] — 2026-09-03

### Fixed

- **`language: "javascript"` handlers ran the source the docs document.**
  A handler written the documented way — `export function handle(...)` — was
  accepted by `create_route`, listed as a healthy route, and then returned 500
  on every request with `engine: syntax error in handler source`. The engine
  evaluates handler source as a *script*, where a top-level `export` is a
  syntax error, and JavaScript reached it without passing through the
  transpiler that removes one. The identical source declared `typescript`
  worked, which is what made a broken route set look fine: five dead routes in
  a ten-route group, with nothing in the listing to say so.

  Both languages now go through one pipeline. Three consequences:

  - **Create-time validation is predictive.** `javascript` previously got no
    validation at all — not even a syntax check — so the only way to discover
    a dead route was to call it. Anything that cannot produce a callable
    `handle` is now `compile_failed` with diagnostics at create/patch time,
    on every surface. If the host accepted your handler, it runs.
  - **Every export form that names `handle` works**, under either language:
    `export function handle`, `export async function handle`,
    `export const handle = ...`, `export default function handle`,
    `export { handle }`, or no export at all. Previously only the exact
    literal `export function handle` was handled, and only for TypeScript —
    the other spellings passed create and failed at request time too.
  - **`import` and anonymous `export default` are rejected** with a message
    naming the problem, instead of failing inside the engine. Handlers run
    with no module loader; inline what they need.

  `language` now selects only the operator-facing label and the UI's syntax
  mode. No migration: stored handler source is unchanged, and source that
  worked before still works.

- **`wait_for_request` and `tail_journal` honour the timeout you asked for.**
  Both rebuilt their timeout on every received event. The journal bus is
  host-wide and filtering happens after the receive, so *any* event restarted
  the clock — including events for groups the caller never asked about. A
  caller asking for 30 seconds could block as long as somebody else's mock
  stayed busy. `wait_for_request` now computes one deadline for the whole
  call, and `tail_journal` resets its idle window only on a match. The tool
  descriptions already promised this, and schemars publishes them verbatim as
  the `tools/list` schema an agent reads before calling.

- **Long MCP waits survive intermediaries.** `wait_for_request` and
  `tail_journal` can block for up to 300 seconds and previously sent nothing
  until they returned. A call that silent is indistinguishable from a hung
  server to anything applying a read timeout — the client's own first-byte
  and idle timers, a reverse proxy — so it got cut well short of the
  requested bound. Both now emit a `notifications/progress` heartbeat
  immediately and then every 15 seconds while waiting. Only when the caller
  supplied a `progressToken`: without one, the response stays on the plain
  JSON fast path exactly as before.

- **`summarize_workspace` reports a real `recent_unmatched_count_5m`.** The
  field shipped hardcoded to `0` with a note that a follow-up would wire it;
  this is that follow-up. Scoped like the group list beside it — an admin
  counts every group's unmatched, a tenant only their own. The walk is capped,
  so a junk-traffic flood cannot put an unbounded read behind a workspace
  summary; past the cap the number is a floor rather than a wrong answer.

## [0.1.1] — 2026-08-28

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
  attached — so a replica nobody is watching deserializes nothing. A tail also
  sees its own replica's traffic immediately, without waiting for the
  subscription to come up; events carry the id of the journal that produced
  them so the origin does not then receive its own back. Existing subscribers
  are untouched; only the feed changed. Note that cross-replica tailing
  requires SUBSCRIBE, not just PUBLISH.
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

### Fixed

- **The container image declares a numeric user.** It was `USER wm`, a
  name — and Kubernetes does not read an image's `/etc/passwd`, so under
  `runAsNonRoot: true` the kubelet cannot verify the user is non-root and
  refuses to start the container. The Helm chart sets `runAsNonRoot`, so
  every pod would have failed with `CreateContainerConfigError`. Now
  `USER 10001:10001`, the same identity in a form that can be checked from
  outside the image, with the uid stated in the chart as well.

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

[Unreleased]: https://github.com/einarfd/wiremirage/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/einarfd/wiremirage/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/einarfd/wiremirage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/einarfd/wiremirage/releases/tag/v0.1.0
