---
name: wm-dev
description: Workflow guide for developing WireMirage itself (the Rust host, CLI, MCP server, and WIT contract). Use when modifying code in this repository, drafting ADRs, or changing the script API.
---

# Developing WireMirage

This skill teaches Claude how to work effectively *on the WireMirage
codebase*. It is distinct from the product skill at `skill/wiremirage/`,
which is shipped to *users* of WireMirage.

## Where things live

- **Code:** Cargo workspace under `crates/`.
  - `wm-core` — shared types, REST client, auth (used by `wm-cli` and `wm-mcp`).
  - `wm-host` — long-running Rust server (axum + wasmtime + Valkey).
  - `wm-cli` — `wm` CLI binary.
  - `wm-mcp` — MCP server.
- **Compiler sidecar (Node, not Rust):** `compiler/typescript/`. Hono
  HTTP server + jco/componentize-js. Built as its own Docker image. Not
  in the cargo workspace; uses npm + tsc + vitest.
- **WIT contract:** `wit/wiremirage.wit` at the repo root — the *verbatim*
  mirror of `script-api-wit.md` in Arkiv. Treat the Arkiv doc as the source
  of truth; if the contract has to change, update the doc first (with an
  ADR if it's a real decision), then mirror it here. `wasm-tools component
  wit <file>` validates and pretty-prints WIT.
- **Wasm guest fixtures:** `crates/wm-host/tests/fixtures/<name>/` —
  standalone crates (own `Cargo.toml` + `Cargo.lock`, **not** workspace
  members). The host's `build.rs` compiles each to `wasm32-unknown-unknown`,
  runs `wasm-tools component new`, and exposes the resulting path as env
  var `WM_FIXTURE_<NAME>_COMPONENT` (e.g.,
  `WM_FIXTURE_ECHO_HANDLER_COMPONENT`) which tests read via `env!()`. To
  add a new fixture, drop a crate under `tests/fixtures/<name>/` and add
  `<name>` to the `fixtures` array in `crates/wm-host/build.rs`.
- **Common commands:** `justfile` at the repo root.
- **Specs (canonical):** Arkiv workspace named `wiremirage`. Find the ID via
  `mcp__claude_ai_Arkiv__search_workspaces` (query `wiremirage`). Read with
  `mcp__claude_ai_Arkiv__read_file` / `read_files` / `get_file_tree`.

## Required tooling

In addition to stable Rust + clippy + rustfmt (already pulled in via
`rust-toolchain.toml`):

- `wasm32-unknown-unknown` target — `rustup target add wasm32-unknown-unknown`
- `wasm-tools` CLI — `cargo install wasm-tools`
- `just` — `cargo install just`
- **Docker** — needed only for `just check-all` / `just test-valkey`. The
  testcontainers-rs sync runner spins up `valkey/valkey:8` for the tier-3
  storage suite. CI already has Docker; locally you only need it if you
  want to exercise the Valkey backend.

CI installs all four. Locally, install once.

## Standard loop

1. **Read the spec before changing behavior.** Start at `index.md`, drill
   into the doc the change touches (`route-model.md`, `storage-model.md`,
   `script-api-wit.md`, `cli-design.md`, etc.) and the relevant ADR.
2. **Make the code change.** Prefer editing existing files over adding new
   ones. Follow the rest of the repo's style (rustfmt, clippy `-D warnings`).
3. **Run `just check`.** Fix what's broken before reporting. If the change
   touches storage, also run `just check-all` to exercise the tier-3
   Valkey suite.
4. **If the code conflicts with the spec, surface it.** Either propose a
   spec update (via `/new-adr` if it's a real decision) or revise the code.
   Don't silently diverge.

## Storage backend selection

The host requires `WM_STORAGE` to be set (no silent fallback). Values:

- `memory` — in-process backing, state lost on restart. Logged as a
  warning at startup.
- `redis://host:port[/db]` — Valkey or other Redis-compatible service.
- `rediss://host:port[/db]` — same, with TLS.

Tests opt into the in-memory backend explicitly via `Storage::in_memory()`
or `WM_STORAGE=memory`; tier-3 tests start a real Valkey container and
use `Storage::valkey(url)`. **Don't introduce a default-to-memory
fallback in production paths** — fail-fast on missing config is a
deliberate project convention.

## Compiler sidecar

`POST /__api/routes` accepts two body shapes (per `rest-api.md`):
pre-compiled `language: "wasm"` uploads go straight through; source-based
`language: "typescript"` requests are forwarded to the sidecar via
`CompilerClient::compile`. Sidecar is reachable via `WM_COMPILER_URL`;
unset → source requests return `compile_failed` ("compiler not
configured"), pre-compiled uploads still work.

The sidecar is *built* as part of the host's tier-3 sidecar tests
(`just test-sidecar`) — those tests start a real container via
testcontainers-rs against a locally-built `wiremirage/compiler-typescript:dev`
image. CI builds the image as a step before running those tests; locally
`just test-sidecar` does the same.

When changing the WIT contract, the sidecar's image must be rebuilt
because the WIT directory is COPYed in at image-build time. The `dev`
tag is a moving target — always rebuild after a WIT change before
running `just test-sidecar`.

## Auth (slice 5)

Bearer tokens gate every `/__api/*` endpoint. Mock-traffic dispatch
(everything not under a reserved prefix) stays open by design — SUTs
don't have tokens. The unauthenticated probes `GET /__health` and
`GET /__ready` are explicitly public.

- **Bootstrap.** On first startup, set `WM_BOOTSTRAP_TOKEN=wmt_...`. The
  host creates an admin user named `bootstrap` whose API token is the
  supplied plaintext. Subsequent starts with the same env var are
  no-ops; rotate by deleting the bootstrap user or revoking via
  `/__api/tokens`. The host **refuses to start** if no users exist and
  `WM_BOOTSTRAP_TOKEN` is unset — this prevents a fresh deployment from
  silently coming up with no way to authenticate.
- **Token shape.** Plaintext: `wmt_<base64-url-no-pad-of-32-random-bytes>`,
  per ADR-0012. Hashed at rest as SHA-256(plaintext); plaintext appears
  exactly once, in the response to `POST /__api/tokens`.
- **Test harness.** `crates/wm-host/tests/api_routes.rs` calls
  `Auth::bootstrap_admin("bootstrap", "wmt_test_bootstrap_token")` and
  builds a default `reqwest::Client` carrying that token. Tests that
  drive 401 / 403 paths construct their own clients via
  `Harness::unauthenticated_client()` or `Harness::provision_user()`.
  `start_with_seeded_route` in `tests/http_smoke.rs` and the Valkey /
  sidecar integration tests do the same.
- **Ownership.** `Route` records carry `owner_id` (set from `auth.user_id`
  at create time). DELETE requires `route.owner_id == caller || caller.is_admin`;
  unauthorized callers get 403 `forbidden`. PATCH and admin "act on behalf of"
  flows for tokens land later.
- **User CRUD.** `/__api/users` exposes POST/GET/PATCH/DELETE plus
  `GET /__api/users/me`. Admin-only for cross-user actions; any authed
  caller can read their own record (via `me` or `GET /{name}` when
  `name` is theirs). Three guardrails on destructive ops:
  (a) an admin **cannot delete themselves** (self-lockout protection),
  (b) the system **cannot drop below one admin** (refuse last-admin DELETE
  or PATCH-to-non-admin), (c) a user that **owns routes cannot be deleted** —
  admins clean up the routes via `/__api/routes/...` first.
  PATCH today only mutates `is_admin`; rename is deferred (it overlaps
  with the user-merge operation in the design docs).
- **Token cascade.** `Auth::delete_user` cascades the user's tokens but
  does *not* touch their routes — the API layer enforces the
  refuse-when-owns-routes rule before calling in.

The legacy `WM_INSECURE_NO_AUTH` gate is gone. Don't reintroduce it.

## Observability (slice 6)

OTel via the standard OTLP/gRPC exporter, opt-in. The host always logs
JSON to stderr; OTel spans flow only when `OTEL_EXPORTER_OTLP_ENDPOINT`
is set. There is no localhost:4317 fallback — operators who don't run a
collector get a clean stderr-only experience.

- **Init.** `wm_host::telemetry::init()` builds the layered subscriber
  and returns a `TelemetryGuard` that the binary's `main` holds for the
  process lifetime. Drop / `shutdown()` flushes in-flight spans; `main`
  hooks SIGTERM/Ctrl-C so the last batch reaches the collector.
- **What's instrumented.** `dispatch_inner` (the request entry span,
  fields: `http.method`, `route.matched_pattern`, `route.id`,
  `outcome`); `wasmtime.instantiate` + `wasmtime.call_handle` as
  children of dispatch; `Auth::authenticate`; `Registry::create_route`
  / `delete_route`; `CompilerClient::compile`. **Avoid** putting the
  raw URL `path` in span attributes — path-param values explode
  cardinality. Use `route.matched_pattern` instead.
- **Propagation.** W3C `traceparent` is extracted from incoming axum
  headers (`HeaderExtractor` adapter) and applied as the dispatch
  span's parent. Outbound sidecar calls inject `traceparent` via
  `HeaderInjector` so the sidecar — once instrumented — chains under
  our span. Both adapters live in `telemetry.rs`.
- **What's not in slice 6.** Metrics (request count, latency
  histogram, fuel) are deferred until we feel the lack. Sidecar OTel
  is out of scope — it's a slim Hono app whose latency is dominated
  by the actual compile work, which our compile span already times
  end-to-end. The per-request *journal* in Valkey (agent-debugging
  surface) is its own future slice; OTel and the journal are
  complementary, not redundant.

## Per-request journal (slice 7)

Every successful or failed mock-traffic dispatch writes a JSON-encoded
record to Valkey, plus an `unmatched:*` record for any request that
reached `dispatch_inner` and didn't match a user route. Reserved-prefix
404s (typos under `/__api/*`, `/__ui/*`, etc.) are intentionally **not**
journaled — those go to stderr/OTel as host operational logs.

- **Module:** `crates/wm-host/src/journal.rs`. `Journal::record_handled`
  / `record_unmatched` write the record; `list_for_group(group_id,
  ListCursor)` / `get(group_id, n)` / `list_unmatched` / `get_unmatched`
  read it back. Cursor pagination via `?before={n}&limit={n}`, capped
  at 100 entries per page, newest-first.
- **Endpoints:** `GET /__api/journal/{group}` (list, admin or any
  group-route owner), `GET /__api/journal/{group}/{n}` (one),
  `GET /__api/unmatched` (list, admin-only), `GET /__api/unmatched/{n}`
  (one). Group reference accepts name or ULID.
- **Storage layout:** `journal:{group_ulid}:{ulid}` is a JSON blob with
  TTL; `journal:by-number:{group_ulid}:{n}` indexes per-group sequence;
  `unmatched:{ulid}` plus `unmatched:by-number:{n}` and
  `unmatched:counter` for the host-wide log.
- **TTL:** hardcoded at 1h for both record types in slice 7. Env-var
  configurable later. The in-memory backend treats `Bucket::set_ttl` as
  a no-op so tests don't depend on wall-clock expiry; tier-3 Valkey
  tests verify real TTL when needed.
- **Body truncation:** 16 KiB for handled records, 4 KiB for unmatched.
  Both carry `body_truncated: bool` and `original_body_size: usize` so
  consumers can flag what they're missing.
- **Resource fields:** `wall_clock_ms` is real; `fuel_consumed` and
  `memory_peak_bytes` are `0` placeholders until per-route resource
  limits land. Schema is stable so consumers don't migrate later.
- **Trace ID:** the dispatcher pulls the W3C trace_id from the inbound
  `traceparent` header (via the OTel propagator installed in
  `telemetry::init`) and stamps it on the record. Tests that exercise
  this path call `wm_host::telemetry::install_propagator()` because the
  global subscriber is set-once and the test harnesses don't run
  `init`.
- **What's deferred:** SSE tail (`/__api/journal/{group}/tail`),
  near-misses computation on unmatched records (currently `[]`),
  configurable TTLs, OTel logs export.

## Group lifecycle (slice 8)

Groups are first-class lifecycle units. Every route, kv/gkv key, and
journal entry has a parent group; when the group goes away (explicit
DELETE *or* Valkey TTL expiry), the children go with it.

- **Endpoints:** `POST/GET /__api/groups`,
  `GET/PATCH/DELETE /__api/groups/{group}`,
  `POST /__api/groups/{group}/refresh`,
  `DELETE /__api/groups/{group}/state`,
  `DELETE /__api/groups/{group}/journal`. Owner-or-admin for per-group
  actions; non-admin list filters to owned groups.
- **TTL.** Default 24h, max 30d, configured per-group. `sliding_ttl`
  defaults to `true` — every successful route match in `dispatch_inner`
  bumps the group's TTL (best-effort; a Valkey hiccup logs and
  continues). Implicit groups (created when a route is POSTed without a
  `group:` reference) inherit the same defaults plus the route
  creator as owner, so they live as long as traffic flows.
- **Cascade.** `Registry::cascade_delete_group(group_id)` is the
  single cleanup path. Wipes routes (and their indexes + per-route
  kv namespace), gkv namespace, journal entries, counters, and the
  group record + indexes. Idempotent: every Valkey op is a no-op if
  the target is gone, so multiple sweepers (or a sweeper racing an
  explicit DELETE) can't corrupt state.
- **Sweeper** (`crates/wm-host/src/lifecycle.rs`). Runs every 30s by
  default. Walks `route:all`, dedupes by group_id, cascades any group
  whose `group:{ulid}` no longer resolves. `Sweeper::single_pass()` is
  exposed so tests can drive it deterministically; the spawn variant
  picks up via tokio's `interval`.
- **Multi-host caveat:** the sweeper invalidates only the local
  in-memory route table cache. Other hosts in a multi-host deployment
  serve stale routes from their caches until they restart or run
  their own sweep on the same group. Proper fix is Valkey keyspace
  notifications (deferred).
- **Deferred for slice 8:** rename, group export, `GET /state`,
  keyspace notifications. Workaround for rename: create new group +
  recreate routes + delete old; user can also use ULID as a stable
  cross-rename handle.

## CLI (slice 9)

`wm-cli` is a thin shell over `wm-core::Client`. Anything HTTP lives
in `wm-core`; the CLI binary handles argument parsing, output
formatting, and exit-code mapping.

- **Layout.** `wm-cli/src/cli.rs` — clap derive tree.
  `wm-cli/src/handlers.rs` — `dispatch(args)` plus per-command
  functions; this is also where exit-code mapping lives
  (`exit_code_for(&ClientError)`). `wm-cli/src/format.rs` — pure
  rendering (human tables + JSON), no I/O beyond `println!` /
  `eprintln!`.
- **Auth and host.** Global flags `--host` (env `WM_HOST`, default
  `http://localhost:8080`) and `--token` (env `WM_TOKEN`, no default)
  are wired via clap's `env = ...`. `wm health` and `wm version`
  fast-path past the auth check; everything else without a token
  exits `4` with a "no token" hint on stderr.
- **Output format.** Human is the default; `--json` switches to
  pretty-printed JSON. The JSON shape is the wire shape — `wm-core`
  serializes the same response structs the host produced. Tables in
  human mode are column-aligned via the in-house
  `print_table<const N: usize>` helper (no extra dep).
- **User-Agent.** `wm-core::DEFAULT_USER_AGENT` is
  `wm-cli/{CARGO_PKG_VERSION}`. Future tooling
  (`wm-mcp`, web UI) builds the client via `ClientBuilder::with_user_agent`
  to identify themselves separately in host logs and OTel spans.
- **Exit codes.** `0` ok, `1` generic, `2` clap usage,
  `4` auth (401 / no token), `5` not-found, `6` conflict.
  `ClientError` → exit code is the single source of truth; renderers
  shouldn't repeat it.
- **Tests.** Three tiers, mirroring the rest of the repo.
  Tier 1 — `wm-core/tests/client_smoke.rs` builds an in-process
  axum router that mimics `/__api/*` and asserts request bodies,
  query strings, headers (including User-Agent), and how response
  shapes round-trip. Cheap, fast, the bulk of coverage.
  Tier 2 — `wm-cli/tests/wm_core_against_host.rs` boots a real
  `wm-host` in-process via `wm_host::router(AppState::new(...))` and
  drives `wm-core::Client` against it. Catches "the host changed
  the wire shape and the client didn't" regressions.
  Tier 3 — `wm-cli/tests/binary_smoke.rs` execs the actual `wm`
  binary via `assert_cmd::Command::cargo_bin("wm")` against the same
  in-process host. Small (a handful of commands) — enough to catch
  "the binary doesn't start" or "a clap arg got renamed". Use
  `tokio::task::spawn_blocking` to invoke `assert_cmd` from inside
  the tokio runtime; remember `.env_remove("WM_TOKEN")` on tests
  that exercise the no-token path so a developer-set env doesn't
  pollute the test.
- **What slice 9 ships.** `wm health`, `wm version`, full `groups`
  CRUD + `refresh` + `state --clear` + `journal --clear`,
  `routes list/add/show/delete` (source file or pre-compiled wasm),
  `journal list/show`, `tokens list/create/revoke`. Everything
  routes through `wm-core::Client`, so adding a flag in the spec
  generally means: add the field to `models.rs`, route it through
  the client method, surface it in `cli.rs`, render it in
  `format.rs`. No new HTTP elsewhere.
- **What's deferred.** Profiles / dotenv / `--config-file`
  (low blast radius to add later), color, shell completions,
  `--from-file` body input for journal show, `wm journal tail`
  (SSE), `wm match` (probe-without-dispatch), `wm routes update / test
  / state`, admin user CRUD. Each is captured in `cli-design.md`
  (private design doc); pick one and read that doc before extending
  the surface.

## Route table architecture

Three layers in `wm-host`:

- `registry::Registry` — durable CRUD on routes/groups via Valkey or
  in-memory storage. Generates ULIDs, allocates per-group route numbers,
  enforces conflict detection at create time. Single source of truth on
  what routes exist.
- `route_table::RouteTable` — in-memory snapshot of all routes plus a
  cache of compiled `wasmtime::Component`s. Warmed at startup from the
  registry; kept in sync via `refresh_after_create` /
  `refresh_after_delete` calls from the API handlers (single-host
  coherence; multi-host via Valkey keyspace notifications is a later
  concern).
- `server::dispatch` — for each request, checks reserved paths, queries
  the route table, instantiates the matched route's component, populates
  `WitRequest` with `matched_pattern` and `path_params`, calls into the
  guest.

The slice-1 catch-all is gone; every request goes through the table.

## Arkiv discipline

- **Read-only by default.** Don't call `update_file`, `create_file`,
  `edit_file`, `delete_file`, `move_file`, or any other write tool unless
  the user has explicitly asked you to.
- ADRs are written via the `/new-adr` slash command, which handles
  numbering, format, and index updates.
- The workspace is private; don't include the workspace ID in any file
  that's intended for public-facing distribution (README, package
  metadata, published docs).

## ADR-driven decisions

Significant design decisions go in an ADR before implementation. ADRs live
in Arkiv at `adrs/NNNN-slug.md`. Conventions are documented in
`adrs/index.md` — follow them.

Numbers are sequential and never reused, even when an ADR is superseded.
Superseded ADRs are rewritten in place with a `Supersedes ADR-NNNN v1`
note at the top.

## WIT contract

The script API contract lives in `script-api-wit.md` (Arkiv) and is
mirrored verbatim in `wit/wiremirage.wit` in this repo. It is high-stakes
— every supported language's bindings depend on it. Before changing the
WIT:

- Read [[adrs/0003-component-model.md]] and the current
  `script-api-wit.md`.
- Update the Arkiv doc first; mirror to `wit/wiremirage.wit` as a separate
  step.
- Bump `bindings_version` per the protocol described there.
- Coordinate with the user on the migration story; don't ship a breaking
  change without an ADR.

The host generates Rust bindings from this file via
`wasmtime::component::bindgen!` in `crates/wm-host/src/bindings.rs`. The
`with` clause maps the WIT `bucket` resource to the concrete `MemBucket`
type. To inspect what the macro generates, `cargo expand -p wm-host --lib
bindings`.

## Conventions worth knowing

- **Latest stable Rust, edition 2024.** No MSRV pin; if a new stable Rust
  breaks something, fix it forward rather than holding back.
- **Clippy `-D warnings`** is enforced in CI. Fix lints; don't `#[allow]`
  them without a comment explaining why.
- **`workspace = true`** for `edition`, `license`, `repository`, `authors`
  in each crate's `Cargo.toml`. Shared deps go in `[workspace.dependencies]`.
- **Identifiers:** internal ULIDs, external scoped slugs (`stripe-mock/7`).
  See [[adrs/0016-ai-friendly-identifiers.md]].
- **Cargo.lock is committed** (binary project).

## What not to do

- Don't add `_unused` renaming, "kept for backwards compat" shims, or
  decorative scaffolding when refactoring — delete dead code.
- Don't introduce a new top-level dependency without checking
  `[workspace.dependencies]` first.
- Don't skip CI hooks (`--no-verify`, etc.) without explicit user request.
- Don't write to Arkiv without explicit user request.
