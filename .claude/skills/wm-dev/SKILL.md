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
  - `wm-core` — shared types, REST client, auth (used by `wm-cli`).
  - `wm-host` — long-running Rust server (axum + wasmtime + Valkey).
    The MCP service lives under `wm-host/src/mcp/` (no separate crate
    — see slice 10 below for the rationale).
  - `wm-cli` — `wm` CLI binary.
- **Product skill:** `skill/wiremirage/SKILL.md` + `scripts/`, plus
  `skill/wiremirage-debug/SKILL.md`. These ship to *users* of
  WireMirage (per ADR-0015) and describe the CLI workflow — distinct
  from the dev skill in this file. Tightly coupled to the current
  CLI; keep updated alongside CLI changes.
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
4. **If the change touched the CLI surface or handler API, update the
   product skill.** `skill/wiremirage/SKILL.md` and any affected
   `scripts/*.sh` describe the current CLI verbatim. Renamed flags,
   added subcommands, changed handler signatures, new gotchas — fold
   them in alongside the code change. `skill/wiremirage-debug/SKILL.md`
   gets the same treatment when the diagnostic primitives change.
5. **If the code conflicts with the spec, surface it.** Either propose a
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

## MCP server (slice 10)

The MCP server is a `mcp/` module inside `wm-host`, mounted onto the
host's axum router at `/__api/mcp` via `rmcp::transport::
StreamableHttpService`. It is *not* a separate crate — that would
have created a circular dep (`wm-mcp` would need `AppState` from
`wm-host`; `wm-host` would need `wm-mcp` to mount it). Folding the
MCP code into `wm-host` keeps the dep graph clean while still
allowing tools to be added/tested in isolation.

- **Layout.** `wm-host/src/mcp/`:
  - `mod.rs` — `pub fn router(state) -> axum::Router` that mounts
    `/__api/mcp` with the bearer-token middleware.
  - `auth.rs` — middleware that reuses `Auth::authenticate` and
    inserts the resolved `AuthContext` into request extensions.
  - `server.rs` — `WmMcpServer` struct holding `Arc<AppState>` plus
    the composed `ToolRouter`. Implements `ServerHandler` via
    `#[tool_handler(router = self.tool_router)]`.
  - `context.rs` — `auth_from(parts) -> AuthContext` plus
    `ensure_group_owner_or_admin` and `ensure_route_owner_or_admin`.
  - `error.rs` — registry/journal error → rmcp `ErrorData` with our
    design-doc codes (`not_found` / `forbidden` / `validation_failed`
    / `conflict` / `internal_error`) in the structured `data` field.
  - `tools/{identity,discovery,groups,routes,state}.rs` — one
    `#[tool_router(router = <name>_router, vis = "pub(crate)")]`
    impl block per domain. Composed in `WmMcpServer::new()` via `+`.
- **Adding a new tool.** (1) Define `Args` and `Result` structs with
  `#[derive(Serialize, Deserialize, JsonSchema)]`. (2) Add an
  `async fn` to the appropriate domain's impl block, attributed
  `#[tool(name = "...", description = "...")]`. The fn signature is
  `(&self, Extension(parts): Extension<http::request::Parts>,
  Parameters(args): Parameters<Args>) -> Result<Json<Result>,
  ErrorData>`. (3) Pull auth via `auth_from(&parts)?`. (4) Call into
  `self.state.routes().registry()` etc. and map errors via the
  helpers in `error.rs`. (5) Update the `expected` list in
  `mcp::tests::server_exposes_all_thirteen_slice_ten_tools` plus the
  tier-2 `mcp_e2e.rs` count assertion if you've truly added one.
- **Auth flow.** The streamable-HTTP transport copies
  `http::request::Parts` (including its `extensions`) into rmcp's
  per-request context. Our axum middleware authenticates the bearer
  token and inserts `AuthContext` into `parts.extensions` *before*
  the rmcp service sees the request. Tools then pull it out with
  `auth_from(&parts)`. Stdio transport doesn't have this plumbing
  yet — see "What's deferred" below.
- **rmcp version.** Pinned to `rmcp = "1.6"` (1.6.0 is the first
  stable release; was `0.16` previously). Workspace dep enables
  `server`, `macros`, `transport-streamable-http-server`,
  `schemars`. Tier-2 tests pull in `client`,
  `transport-streamable-http-client`,
  `transport-streamable-http-client-reqwest`, `reqwest` as
  dev-dependencies.
- **Reqwest version note.** rmcp's `transport-streamable-http-client-
  reqwest` feature pins reqwest 0.13. We bumped the workspace
  reqwest from 0.12 to 0.13 in slice 10 so both reqwest versions
  don't appear in the tree (the `StreamableHttpClient` trait impl
  applies only to a single reqwest version, so a mismatch shows up
  as a baffling "trait not implemented" error). If you ever need to
  hold reqwest at 0.12 again, the right answer is to find a
  conditional path through rmcp's transport types rather than work
  around the version drift in a test file.
- **Linker memory.** Adding rmcp's transitive deps pushed the linker
  near OOM on small VMs. If `cargo test -p wm-host` fails with
  `linking with cc failed: ld terminated with signal 9`, retry with
  `CARGO_BUILD_JOBS=2`. Not yet wired into the justfile because the
  default works on most boxes.
- **Tests.**
  - Tier 1 — `mcp::tests` in `mcp/mod.rs`: tool count + name set,
    every tool has `type: object` input schema, canary tools
    advertise their fields. Pure rust, no network.
  - Tier 2 — `crates/wm-host/tests/mcp_e2e.rs`: rmcp client speaks
    streamable HTTP to in-process wm-host. Covers `list_tools`,
    `who_am_i`, group create/show round-trip, validation error
    propagation, bad-token rejection.
  - Tier 3 — `crates/wm-host/tests/mcp_stdio.rs`: WmMcpServer over
    in-memory duplex pipes. Covers protocol-level handshake +
    `list_tools` only. Tool *invocation* over stdio is intentionally
    not exercised because our auth flow expects HTTP request parts —
    a stdio session would need its own auth bridge.
- **What slice 10 ships.** 13 tools: `who_am_i`,
  `summarize_workspace`, `list_recent_unmatched`, `list_groups`,
  `show_group`, `create_group`, `delete_group`, `refresh_group_ttl`,
  `list_routes`, `show_route`, `create_route`, `delete_route`,
  `clear_group_state`.
- **What's deferred from slice 10.** `find_route`, `update_route`,
  `dry_run_route`, per-route state ops need their REST endpoints to
  exist first; bundle them with their host additions. ~~`create_route`
  over MCP currently accepts `language: "wasm"` only; source-based
  TS creation routes through the CLI/REST until we decide whether
  agents should ever post inline source through MCP.~~ **Closed by
  slice 42** — MCP create + update accept source-language. Stdio
  production deployment + auth bridge for stdio sessions are out of
  scope.

## Dispatch body limit (slice 45 / F-2)

Closes audit finding F-2: the mock-dispatch path used to call
`body.collect()` with no size limit, letting any unauthenticated
caller buffer arbitrary bytes into host memory.

- **`server.rs`**: new `MAX_DISPATCH_BODY_BYTES = 10 *
  1024 * 1024` (10 MiB) per
  `storage-model.md::limits.request_body_size`.
  `read_body` now takes a max-bytes arg and returns a
  `BodyReadOutcome` enum (`Ok(Vec<u8>)` or `TooLarge`).
  `dispatch_inner` maps `TooLarge` to a 413 response
  with the trace ID stamped on the headers. The 413
  path deliberately doesn't write to the unmatched
  journal — a junk flood shouldn't pollute logs.
- **`api.rs`**: new `MAX_API_BODY_BYTES = 16 * 1024 *
  1024` (16 MiB) applied as a
  `DefaultBodyLimit::max(...)` layer on the
  `/__api/*` router. Lifts axum's 2 MiB default so
  wasm uploads on `POST /__api/routes` + `PATCH
  /__api/routes/{g}/{n}` aren't artificially cut off.
  Base64 expansion (~33%) means ~12 MiB of raw wasm
  fits — comfortably above componentize-js's typical
  1-3 MiB output. Auth-gated, so the higher limit
  doesn't expose a public DoS surface.
- **Tests** (`tests/http_smoke.rs`):
  - `dispatch_rejects_body_above_limit_with_413` — 12
    MiB POST → 413 with a body that mentions the limit.
  - `dispatch_accepts_body_below_limit` — 1 MiB POST
    succeeds via the echo handler.

## Secure cookies + trusted-proxy bool + hardening doc (slice 44)

Closes audit findings F-3, F-4, and F-5 from the
`.audit/security-audit-2026-05-17.md` report. Two new
boolean env flags + a production-hardening section in the
README.

- **`WM_SECURE_COOKIES`** — appends `Secure` to the
  `wm_session` and `wm_csrf` cookies. Default off so
  dev workflows over plain HTTP keep minting usable
  cookies (browsers drop `Secure` cookies on non-TLS).
  Deployments behind a TLS edge MUST set it.
- **`WM_TRUST_FORWARDED_HEADERS`** — honors
  `X-Forwarded-For` for the per-IP login throttle. Default
  off: when the header isn't trusted, the loopback
  placeholder is used (the current behavior pre-slice-44
  for missing-header callers, just now applied
  uniformly). Only enable when a reverse proxy you
  control is the only thing that can reach the host —
  otherwise an attacker can spoof the throttle bucket
  via header injection.
- **F-5 doc** — README gains a "Production hardening"
  section listing both flags, bootstrap-token rotation
  (`openssl rand -hex 32` → log in, mint operator
  token, delete bootstrap user), strong
  `SESSION_SECRET` generation, edge-side headers
  (HSTS, nosniff, strict CSP feasible since Ace is
  vendored), and the localhost-bind recommendation.

**Why a bool instead of a CIDR list for F-4?** The audit
sketched a `WM_TRUSTED_PROXIES` CIDR list, but plumbing
`ConnectInfo<SocketAddr>` through the router to make
the CIDR check meaningful would touch every test
harness — a comment in `auth_api.rs` already calls that
out as deferred. The bool achieves the same goal (only
trust XFF when the operator says so) without the
plumbing churn, and the deployment shape (host binds
to localhost, Caddy is the only thing that can reach
it) doesn't need CIDR granularity.

- **`server.rs::AppState`**: gains `secure_cookies: bool`
  and `trust_forwarded_headers: bool` plus matching
  `with_*` builders and getters.
- **`main.rs`**: new `parse_env_bool` helper reads the
  flags from env at boot.
- **`auth_api.rs`**: `format_set_cookie` and a new
  `format_clear_cookie` (for logout) take a `secure:
  bool` param. `client_ip` takes `trust_forwarded: bool`.
  Router constructor now takes `AppState` so the CSRF
  middleware can read the flag.
- **`ui/csrf.rs`**: `build_set_cookie` takes the flag.
  `csrf_middleware` signature gains
  `State<AppState>`. Both router wirings switch from
  `from_fn` to `from_fn_with_state`.
- **Tests** (`tests/local_auth_e2e.rs`):
  - `cookies_have_no_secure_flag_by_default`
  - `cookies_carry_secure_flag_when_enabled` (both
    `wm_csrf` and `wm_session`)
  - `forwarded_for_ignored_by_default_so_throttle_collapses_to_loopback`
    — five failed logins with rotating XFF IPs still
    locks the loopback bucket; sixth attempt from a
    fresh XFF IP returns 429.
  - `forwarded_for_honored_when_explicitly_trusted` —
    same setup but with the flag on; sixth attempt from
    a different XFF IP succeeds with 303.

## MCP update_group (slice 43)

Closes the MCP/CLI/UI parity gap on group editing. CLI
(`wm groups update`) and UI (group-detail edit-TTL
disclosure) both supported TTL + sliding-flag changes;
MCP did not. Adds `update_group` so agents can flip the
two mutable fields without dropping to the CLI.

- **`mcp/tools/groups.rs`**: new `UpdateGroupArgs { group,
  ttl_seconds, sliding_ttl }` (both numeric fields
  optional, at least one required). Handler is a thin
  wrapper around `Registry::patch_group` — same
  validation + Valkey-TTL re-arming the REST PATCH does.
  Owner-or-admin via the shared `ensure_group_owner_or_admin`
  helper. No need to extract a `patch_group_core` like
  routes had — `patch_group` is already self-contained.
- **Tool-list expected counts**: `mcp::tests::server_exposes_all_expected_tools`
  and `tests/mcp_stdio.rs::list_tools_works_over_stdio_duplex`
  bumped to 22. The `tests/mcp_e2e.rs::list_tools_returns_all_expected_tools`
  expected list also grew.
- **Tests** (`tests/mcp_e2e.rs`):
  - `update_group_changes_ttl_and_sliding_flag` — happy
    path, both fields changed in one call, verified via a
    follow-up `show_group`.
  - `update_group_rejects_empty_patch` — neither field
    set → `validation_failed`.
  - `update_group_forbidden_for_non_owner_non_admin` —
    non-owner non-admin user can't update an admin's
    group (same gate the REST PATCH applies).
- **Not in scope**: rename and owner-transfer. Same
  carve-out as REST and CLI — comment in
  `registry::patch_group` calls those out explicitly as
  "aren't supported in this slice."

## MCP source-language create + update (slice 42)

The slice-10 / slice-15 wasm-only carve-out on MCP
`create_route` and `update_route` is retired. Both tools
now accept source-language inputs (`typescript` /
`javascript` with a `source` string) in addition to the
existing wasm path. Unblocks agent-driven deployments —
agents no longer need a wasm toolchain to register or
edit a TS/JS handler.

- **`mcp/tools/routes.rs`**: `CreateRouteArgs` gains
  `source: Option<String>`; `UpdateRouteArgs` gains
  `source: Option<String>` plus `language:
  Option<String>` (required when the artifact is being
  swapped). Both handler bodies are now thin wrappers
  that build `CreateRouteBody` / `PatchRouteBody` and
  delegate to `api::create_route_core` / `patch_route_core`.
  Compile, sidecar, conflict-precheck, source-storage,
  and component-validation all live in the shared core
  helpers — MCP and REST go through the same code path.
- **`mcp/error.rs`**: new `map_api_error(ApiError) ->
  ErrorData` propagates `code`, message, and any
  compile-failed diagnostics into the structured `data`
  payload. Mirrors the REST error envelope.
- **Why pub(crate) on the api bodies works**: the MCP
  module lives inside `wm-host`, so `pub(crate)` on
  `CreateRouteBody` / `PatchRouteBody` and their fields
  is enough — no public-API surface change.
- **Tests** (`tests/mcp_e2e.rs`):
  - `create_route_accepts_typescript_source` — happy path through the mock-compiler harness.
  - `create_route_typescript_without_compiler_returns_compile_failed` — no `WM_COMPILER_URL` configured → compile_failed surfaces.
  - `create_route_rejects_source_and_wasm_together` — validation_failed on the both-fields case.
  - `update_route_swaps_typescript_source` — verifies the stored source actually changed via a follow-up `show_route_source` call.
  - `update_route_can_switch_wasm_to_source_and_back` — wasm → TS swap stores source; TS → wasm swap clears it. The same `Some(None)` / `Some(Some(_))` patch semantics the REST PATCH path uses.
- **Test-harness change**: `mcp_e2e.rs` gains a
  `start_with_mock_compiler()` helper mirroring the
  pattern from `ui_source_edit.rs` /  `ui_route_new.rs`
  — a tiny axum server returns canned echo-wasm bytes
  for any `/compile` POST, so source-language tests
  don't need a real componentize-js sidecar.

## Ace Editor for source viewer + editor (slice 41)

The slice-37 read-only `<pre>` and the slice-29 / slice-40
textareas are upgraded to Ace Editor with JS/TS syntax
highlighting, line numbers, and basic auto-indent. Ace is
vendored as a script-tag distribution — no JS bundler.

- **`src/ui/static/ace/`**: vendored from `ace-builds@1.43.5`
  (npm), the `src-min-noconflict` build (UMD-style, no `define`
  / `require` pollution). Files: `ace.js` (core),
  `mode-javascript.js`, `mode-typescript.js`,
  `theme-github_light_default.js`, `theme-github_dark.js`,
  plus the upstream `LICENSE` (BSD). Workers are NOT vendored
  — we set `useWorker: false` since async syntax-error
  checking isn't worth the extra files for our scope.
- **`src/ui/static/wm-ace.js`**: ~80-line bootstrap. Finds
  every `<div data-wm-ace="...">` on the page, replaces its
  content with an Ace instance configured per the data
  attributes (`data-wm-ace` = mode, `data-wm-ace-readonly`
  = viewer, `data-wm-ace-sync` = textarea name to mirror).
  Picks the theme from `prefers-color-scheme` and listens
  for changes so OS theme toggles flip the editor live.
- **`src/ui/static_assets.rs`**: enum-match handler grows
  six new entries (the five Ace files + `wm-ace.js`). The
  match-arm structure is deliberate — the wildcard route
  can't be coaxed into serving arbitrary files out of the
  binary's data segment. Unknown paths under `ace/` still
  404.
- **Templates**:
  - `route_detail.html` swaps `<pre class="source-block">`
    for `<div class="ace-host ace-host--viewer"
    data-wm-ace="..." data-wm-ace-readonly>{{ source }}</div>`.
    The `{% if route.source %}` block is what guards the
    script-tag include — wasm-uploaded routes never load
    Ace.
  - `route_new.html` and `route_source_edit.html` render
    a `<div class="ace-host" data-wm-ace="..."
    data-wm-ace-sync="source">` paired with a hidden
    `<textarea name="source">{{ ... }}</textarea>`.
    wm-ace.js takes the textarea's value as the initial
    editor content, hides the textarea via inline style,
    and re-syncs on every change + on form submit.
- **CSS**: new `.ace-host` (editor, 24rem) and
  `.ace-host--viewer` (read-only, 20rem) under
  `wm.css`. Ace draws its own background; the host
  border + mono fallback font kick in if the script
  fails to load.
- **Why no MCP / CLI affordance change?** This is pure UI
  polish; the wire surface is unchanged. CLI source
  editing was already in via `wm routes update
  --source-file`.
- **Mode dropdown on `/__ui/routes/new`.** Changing the
  language `<select>` live-swaps the Ace mode via a tiny
  `window.wmAce.setMode(host, mode)` helper exposed by
  `wm-ace.js`. The host div is stashed on a property
  (`div._wmAceEditor`) at attach time so callers don't
  need to grovel through Ace internals. An inline
  script at the bottom of `route_new.html` wires the
  select's `change` event to that helper. New callers
  should add named methods to `window.wmAce` rather
  than reach into `_wmAceEditor` directly.
- **Tests:**
  - `tests/ui_smoke.rs::ace_editor_assets_served_with_js_mime` — every vendored script comes back 200 + `application/javascript`; unknown asset under the prefix still 404s.
  - Existing slice-37/40 assertions updated: `data-wm-ace` replaces `source-block` as the marker the test grep'd for.

## Source editing on route detail UI (slice 40)

Makes the slice-37 read-only source card editable. New
`/__ui/routes/{group}/{n}/source/edit` page renders a
textarea pre-populated with the route's stored source;
POST forwards to `api::patch_route_core` which
recompiles via the sidecar and swaps the artifact in
place.

- **`api.rs`**: extracted `pub(crate) async fn
  patch_route_core(state, auth, group, number, body) ->
  Result<Route, ApiError>` from the inline body of
  `patch_route`. REST handler is now a thin wrapper that
  converts the returned `Route` to `RouteResponse`.
  Same shape as the slice-29 `create_route_core` split.
  `PatchRouteBody` and its fields become `pub(crate)`.
- **`ui/mod.rs`**: `route_source_edit_page` (GET) +
  `route_source_edit_submit` (POST), both owner-or-admin
  gated. Routes with `source: None` (wasm uploads) 404
  on both — no source to edit, and recompiling-wasm-from-
  wasm isn't a thing the sidecar supports. On compile
  failure, the form re-renders with `SourceEditError {
  title, message, diagnostics }` and the user's edits
  intact.
- **`templates/route_source_edit.html`**: textarea +
  Save/Cancel + breadcrumb back to route detail. Reuses
  `.source-editor` from slice 29 and `.card--error`
  from existing patterns — no new CSS.
- **`templates/route_detail.html`**: replaces the
  slice-37 "wm routes update ... --source-file ..."
  hint with a real "Edit source" button inside the
  Handler source card's `.action-row`. Button
  suppressed for wasm-uploaded routes (their card shows
  the empty-state line instead).
- **Why no MCP / CLI editing flow?** The CLI already
  has `wm routes update <slug> --source-file <path>`
  from slice 15; this slice is purely the UI
  affordance. MCP `update_route` stays wasm-only per
  the slice-15 decision.
- **Tests** (`tests/ui_source_edit.rs`):
  - `edit_form_renders_with_current_source_for_ts_route`
  - `edit_form_404s_on_wasm_route_without_stored_source`
  - `edit_form_403_for_non_owner_non_admin`
  - `edit_submit_with_new_source_recompiles_and_redirects` — uses the canned-bytes mock compiler trick from `ui_route_new.rs` to exercise the happy path; verifies the updated source renders on the detail page after the redirect.
  - `edit_submit_without_csrf_is_forbidden`
  - `edit_submit_without_compiler_reports_compile_failed` — verifies the no-compiler case re-renders with `compile_failed` + the user's edits preserved.

## MCP near-miss projection on list_recent_unmatched (slice 38)

Backfill of the slice-35 deferral: `list_recent_unmatched`
now ships the slim near-miss list on every entry, so agents
don't need a second REST hop to `/__api/unmatched/{n}` to
see the "Did you mean…?" candidates.

- **`mcp/tools/discovery.rs`**: `UnmatchedSummary` gains a
  `pub near_misses: Vec<UnmatchedNearMiss>` field (the
  host's own type, which already derives `JsonSchema`). The
  map step propagates `r.near_misses` from the journal
  record. `#[serde(default)]` so the wire stays
  forward-compatible if a future record was missing the
  field for any reason.
- **Shape**: identical to what `/__api/unmatched/{n}` and
  the UI already serialise. No new schema. The field is
  present as `[]` (not omitted) when the dispatcher didn't
  find a neighbour — agent code can rely on its shape.
- **Tests:**
  - `tests/mcp_e2e.rs::list_recent_unmatched_includes_near_misses_projection` — seed an unmatched record with one method-mismatch near-miss via `record_unmatched`, assert the MCP response carries `near_misses[0].route` + `reason.kind == "method_mismatch"`.
  - `tests/mcp_e2e.rs::list_recent_unmatched_emits_empty_near_misses_when_none` — seed without neighbours, assert `near_misses` is present as `[]`.

## Source viewer on route detail UI (slice 37)

`/__ui/routes/{group}/{n}` now renders a "Handler source" card
just above the footer. For source-language routes (where slice
36's `route.source` is `Some`) the card shows the stored source
in a read-only `<pre class="source-block"><code>` block. For
pre-compiled wasm uploads (where source is `None`) the card
shows an empty-state line: "No source stored — route was
uploaded as pre-compiled `wasm` (N KiB component)."

- **`ui/mod.rs`**: `RouteDetailRoute` gains
  `source: Option<String>`, populated from `route.source.clone()`
  in the handler. No new endpoint — the source travels with the
  route record we already fetched.
- **`templates/route_detail.html`**: the slice-23 placeholder
  paragraph ("Source viewing + editing from the UI land in
  later slices.") is replaced by the new card. The
  `wm routes update ... --source-file ...` hint stays under the
  source block as a temporary CLI fallback for the still-deferred
  source editor.
- **`static/wm.css`**: new `.source-block` class — mono font,
  bordered, horizontally scrollable, `max-height: 32rem` so a
  long handler doesn't push the rest of the page off-screen.
  Differs from `.source-editor` (which is for `<textarea>`s on
  the create/dry-run forms).
- **Why not a separate GET subroute for the UI?** The source is
  already on the `Route` record after slice 36, and the detail
  page is already owner-or-admin-gated, so wedging in a second
  fetch is pure cost. The dedicated REST/MCP `/source`
  endpoints stay — they're the contract for non-UI callers.
- **Tests:**
  - `tests/ui_detail_pages.rs::route_detail_renders_no_source_stored_for_wasm_upload` — existing wasm-upload route shows the empty-state line and no `<pre class=source-block>`.
  - `tests/ui_detail_pages.rs::route_detail_renders_stored_source_for_source_language_route` — fresh harness registers a TS route with source; detail page renders the source inline.

## Source storage on the registry (slice 36)

The `Route` record gains an `Option<String> source` alongside
`compiled_wasm`. For source-language routes (`typescript`,
`javascript`), we now keep the original handler source the user
posted; for pre-compiled `wasm` uploads, `source` is `None`
(no source ever existed in the host).

- **`registry.rs`**: `Route`, `NewRoute`, and `PatchRoute` gain
  `source: Option<String>` / `Option<Option<String>>` (tri-state
  patch). `write_route` deletes the `source` Valkey field when
  `None` rather than storing an empty string, so re-reads
  cleanly return `None` via `utf8_opt`. `update_route`'s artifact-
  swap branch honours `patch.source`: source-language swap writes
  the new string; wasm swap deletes the field. The Route struct
  list in `delete_route` includes "source".
- **`api.rs`**: `create_route_core`'s match arm now returns a
  4-tuple `(compiled_wasm, language, bindings_version, source)`
  — the source-language branch sets `Some(source)`, the wasm
  branch sets `None`. `patch_route_core` computes
  `source_patch: Option<Option<String>>` alongside the artifact
  decision: wasm swap → `Some(None)`, source-lang swap →
  `Some(Some(src))`, no change → `None`. New endpoint
  `GET /__api/routes/{group}/{number}/source` (owner-or-admin
  gate) returns a `RouteSourceResponse { slug, language,
  source }`. The `compiled_wasm` bytes still never appear on
  list/get; `source` follows the same pattern — only the dedicated
  `/source` endpoint returns it.
- **`mcp/tools/routes.rs`**: new `show_route_source` tool
  (owner-or-admin gate, returns `ShowRouteSourceResult`).
  `create_route` MCP passes `source: None` (MCP create stays
  wasm-only). `update_route` MCP passes
  `source: if compiled_wasm.is_some() { Some(None) } else { None }`
  (wasm swap clears any stored source).
- **`wm-core`**: `RouteSourceResponse` mirrors the host struct.
  `Client::get_route_source(slug)` is the wm-core entry point.
- **`wm-cli`**: `wm routes source <slug>` prints the source to
  stdout in Human mode (or `(no source stored — route was
  uploaded as pre-compiled '{language}')` if `None`); `--json`
  emits the wire shape.
- **Why MCP doesn't accept source on `create_route`/`update_route`.**
  The slice-10 decision is preserved: agents post pre-compiled
  wasm bytes through MCP. The `source` field on the record
  exists for the dedicated viewer/show endpoint, not for round-
  trip source-via-MCP. Source-language create still goes
  through CLI/REST.
- **Tests:**
  - `tests/api_routes.rs::source_is_persisted_for_source_language_routes` — TS source round-trips through `GET /source`.
  - `tests/api_routes.rs::source_is_null_for_wasm_uploaded_routes` — wasm upload → null source.
  - `tests/api_routes.rs::source_updates_on_source_language_patch` — patch with new source swaps in.
  - `tests/api_routes.rs::source_cleared_when_wasm_swapped_in` — patch from source-lang → wasm clears stored source.
  - `tests/api_routes.rs::source_endpoint_forbids_non_owner` — 403 on owner-mismatch.
  - `tests/api_routes.rs::source_endpoint_returns_404_for_unknown_route` — missing → 404.
  - `tests/mcp_e2e.rs::show_route_source_returns_source_for_typescript_route` — MCP returns the source.
  - `tests/mcp_e2e.rs::show_route_source_is_null_for_wasm_route` — MCP returns null for wasm.

## Unmatched near-misses (slice 35)

Closes the last big agent-facing gap from the slice-28 spec
note: `UnmatchedRecord.near_misses` had always been
`vec![]`. The dispatcher now populates it at unmatched-
write time, and the UI / REST / CLI all surface the
suggestions.

- **`route_table.rs`**: split `compute_near_misses(method,
  path) -> Vec<NearMiss>` out of `probe`. The unmatched-
  write path already knows there's no hit, so re-running
  `find_match` would be wasted work. `probe` keeps its
  Hit-or-Miss contract by calling the new helper.
- **`journal.rs`**: `UnmatchedRecord.near_misses` changes
  from `Vec<String>` to `Vec<UnmatchedNearMiss>`. The new
  type carries the `{group}/{n}` slug, the route's path
  + methods, and a reason: either
  `MethodMismatch { expected_methods, got }` or
  `PrefixMatch { segment_index, expected, got }` — same
  two reasons slice 13's `find_route` MCP tool exposes,
  serialised with `#[serde(tag = "kind",
  rename_all = "snake_case")]` so the JSON wire format
  matches `{"kind": "method_mismatch", "expected_methods":
  [...], "got": "..."}`. `NewUnmatchedEntry` gains the
  matching field; `record_unmatched` propagates it onto
  the persisted record. Old Valkey entries with
  `near_misses: []` deserialise fine into the new type
  (empty list matches any element type).
- **`server.rs`**: the dispatcher's unmatched-404 branch
  calls `state.routes().compute_near_misses(method, path)`,
  maps each `NearMiss` (which carries the full `Route`
  including the heavy `compiled_wasm`) through
  `project_near_miss` into the slim `UnmatchedNearMiss`,
  and passes the list into the journal write.
- **`wm-core/src/models.rs`**: parallel `UnmatchedNearMiss`
  + `UnmatchedNearMissReason` definitions so the
  CLI/wm-core client deserialises the new shape. The two
  type families are kept structurally identical; they
  don't share a crate because wm-host doesn't depend on
  wm-core.
- **CLI** (`wm-cli/src/format.rs`): `render_unmatched_list`
  now prints each near-miss as `slug METHOD path —
  reason (details)` instead of the old plain slug. JSON
  output unchanged in spirit but with richer per-entry
  data.
- **UI**: `UnmatchedRow` gains a `primary_hint: Option<…>`
  (first near-miss only, surfaced as a "Did you mean
  …?" line on the list page; empty → "No close
  neighbours."). `UnmatchedDetailView` gains a
  `near_misses` vec of slim view-models that include an
  `explanation: String` rendered via a `From<&Unmatched
  NearMiss>` impl — keeps the reason-formatting in Rust
  rather than smearing match arms through the template.
- **MCP**: in slice 35 the `UnmatchedSummary` projection
  still dropped near_misses (deferred to slice 38). Slice
  38 backfilled it — `list_recent_unmatched` now returns
  `near_misses: []` on every entry, populated when the
  dispatcher found neighbours and present-as-empty
  otherwise.
- **Tests:**
  - `tests/api_routes.rs::unmatched_record_carries_near_misses_for_method_mismatch` — register POST /v1/charges, hit GET /v1/charges, verify the unmatched journal record carries a method-mismatch near-miss with `expected_methods` and `got`.
  - `tests/api_routes.rs::unmatched_record_carries_near_misses_for_prefix_typo` — register /v1/refunds, hit /v1/refund, verify the prefix-match near-miss.
  - `tests/ui_unmatched_pages.rs::unmatched_list_renders_did_you_mean_hint` — synthetic near-miss injected via `record_unmatched`, list page shows "Did you mean" + link.
  - `tests/ui_unmatched_pages.rs::unmatched_list_shows_no_close_neighbours_when_empty` — empty near-miss case.
  - `tests/ui_unmatched_pages.rs::unmatched_detail_lists_near_misses_with_explanation` — detail page lists the reason text.

## Live journal Pause/Resume (slice 34)

The wireframe-since-slice-24 Pause button finally lands.
Pure client-side change in `live_journal.html` — no host
work, no new tests beyond a smoke check that the button
renders.

- **State**: a `paused` boolean and a `buffer` array
  (capped at 500). While paused, the `handled` SSE
  listener pushes to `buffer` instead of calling
  `renderRow`. On overflow we `shift()` the oldest, so a
  long pause + heavy traffic doesn't grow memory but
  the user keeps the most-recent N entries visible on
  resume.
- **Resume flush order**: oldest-first via `renderRow`,
  which prepends — so the newest ends up at the top,
  matching the un-paused ordering exactly. No surprises
  for the operator.
- **Status indicator**: extended to report `paused` /
  `paused · N buffered` / `live` / `disconnected (will
  retry)` based on `paused` + `connected` flags. A small
  `setStatus()` helper centralizes the rendering so the
  three event handlers + the button click all go through
  one place.
- **Button**: rendered only when `can_tail` is true
  (same guard as the SSE script block) — non-admin
  picker-view callers see no button. Initial label
  `Pause`, toggles to `Resume` on click.
- **Tests:** `ui_journal_pages.rs` —
  `live_journal_page_renders_with_group_filter` extended
  to assert the button + initial label;
  `live_journal_picker_view_omits_pause_button` covers
  the non-tail picker path. JS behavior beyond
  rendering can't easily be tier-2-tested without a
  headless browser, which we don't have.
- **Not in this slice**: highlight-on-arrival animation
  for new rows (still deferred per slice-24 notes); the
  buffer-cap value is hard-coded at 500 (small enough
  to be safe, large enough that "step away for a coffee"
  pauses still keep useful data — could be made
  configurable later if anyone cares).

## Dry-run seed state (slice 33)

Lets agents/CLI/UI pre-populate the dry-run snapshot's
`kv:` and `gkv:` before the handler runs. Solves the
"test `if counter > 3`" pain that previously required
driving real traffic first.

- **Core** (`dry_run.rs`): `DryRunRequest` gains
  `kv_overrides: HashMap<String, Vec<u8>>` and
  `gkv_overrides: HashMap<String, Vec<u8>>`. In
  `run_in_snapshot`, after the existing
  `copy_keys_with_prefix` deep-copy of real state and
  before the handler is instantiated, the overrides are
  written via `bucket.set(key, value)` on the dry route
  and group buckets. Order matters: real-state-copy →
  overrides → handler. Overrides win on collision. Real
  state is never touched — overrides land in the
  disposable `dryrun:{run_id}:` namespace and are wiped
  on completion with the rest of the snapshot.
- **REST / wm-core** (`models.rs::DryRunBody`): same
  field shape as the host, `HashMap<String, Vec<u8>>`.
  Serializes as `{"counter": [52, 53]}` in JSON (matches
  the existing `body: Vec<u8>` array-of-ints
  convention). Verbose but consistent.
- **MCP** (`tools/state.rs::DryRunRouteArgs`): uses
  `kv_overrides_b64: Option<HashMap<String, String>>`
  and `gkv_overrides_b64` with explicit base64 string
  values, matching the existing `body_b64` convention.
  `decode_overrides_b64` helper base64-decodes each
  value and surfaces the offending key in the error
  message on bad input.
- **CLI** (`wm routes test`): two new repeatable flags:
  `--kv KEY=VALUE` and `--gkv KEY=VALUE`. UTF-8 only;
  for binary, the REST/MCP base64 path is the answer.
  `parse_override_pairs` trims whitespace around keys,
  rejects missing-`=` and empty keys, and allows `=` in
  the value side (so base64 padding round-trips).
- **UI** (`route_dry_run.html` + `mod.rs`): a third
  "Seed state" card on the dry-run page with two
  textareas (Route kv / Group gkv). Same
  `parse_kv_lines(_, '=')` helper used for headers and
  query; bad input renders inline 400 with the offending
  field preserved.
- **Out of scope**: typed seeds for lists/sets/hashes —
  if a handler does `ctx.kv.list_push("queue", x)` and
  reads back via `list_range`, the workaround is still
  to seed via real traffic. Adding a list/set/hash seed
  schema would need a discriminated union in the
  request and matching write paths in the snapshot
  machinery. Worth its own slice when someone hits the
  wall.
- **Tests:**
  - `tests/api_routes.rs::dry_run_kv_overrides_seed_snapshot_state` — REST happy path against counter_handler, confirms real state stays empty after multiple seeded runs.
  - `tests/mcp_e2e.rs::dry_run_route_with_kv_overrides_seeds_snapshot` — MCP base64 path + a bad-base64 rejection test.
  - `tests/ui_dry_run.rs::dry_run_kv_override_seeds_snapshot` — UI form path.
  - `tests/ui_dry_run.rs::dry_run_bad_kv_override_renders_inline_400` — bad textarea input.
  - `handlers::tests::parse_override_pairs_*` — 5 CLI unit tests covering happy path, key trim, missing `=`, empty key, value-with-equals.

## Dry-run UI page (slice 32)

Surfaces the slice-16 dry-run API at
`/__ui/routes/{group}/{n}/dry-run` as a real full page —
the route-detail footer's "Run dry-run" link is no longer
the slice-30 muted "CLI only" placeholder.

- **Page shape**: form on top (Method dropdown / Path
  text / Headers textarea / Query textarea / Body
  textarea), Response card below when the user has
  submitted. Not a JS modal — keeps the asset surface
  small, deep-linkable, accessible without overlay
  patterns.
- **Form parsing**: `parse_kv_lines(input, sep)` splits a
  multi-line textarea on the first `sep` per line —
  `:` for Headers, `=` for Query. Trims whitespace,
  skips blank lines, errors on missing-separator with
  the offending line in the message. Path must start
  with `/`. Bad form input renders inline 400 with all
  fields preserved.
- **Handler call**: builds a `dry_run::DryRunRequest` and
  calls `dry_run::dry_run(runtime, routes, route, request)`
  directly (no REST round-trip). Re-uses the slice-16
  semantics: snapshot of `kv:` + `gkv:` under a
  `dryrun:{run_id}:` root, handler instantiated against
  the shifted root, snapshot wiped on completion, no
  journal write.
- **Response rendering**: `DryRunResponseView` carries
  status / status_class / duration_ms / snapshot_keys /
  headers / body_text (UTF-8 or `(binary, N bytes)` via
  the shared `body_as_text` helper) / handler_logs /
  error. Inline `.card--error` callout when the handler
  trapped or returned an error.
- **Authz**: owner-or-admin, mirroring REST. CSRF on
  the POST.
- **Out of scope**: path-params override (rarely needed
  in practice; users who care can phrase the route
  pattern + literal path so the handler reads what it
  wants), CodeMirror on Body, file upload for binary
  bodies.
- **Tests**: `tests/ui_dry_run.rs` — 8 tier-2 tests.
  GET form pre-fills from the route's first method +
  path. POST runs the handler, renders status + body
  (verified against `counter_handler`'s `count=1`
  response). Crucially, three back-to-back dry-runs
  leave the route's real `kv:` and the journal
  unchanged. Bad headers / non-`/` path → inline 400
  with form values preserved. Missing CSRF → 403.
  Non-owner → 403. Unknown route → 404.
- **Test churn**: `ui_detail_pages::route_detail_renders_metadata_for_owner`
  swapped its `wm routes test` assertion for one
  checking that the new footer link is present.

## Audit-driven cleanup (slice 31)

A dogfood pass triggered a full wireframe audit (using the
general-purpose subagent against the Arkiv spec). Five small
commits landed under the slice-31 banner to close the gaps
that didn't have a "Slice N state" deferred-note already.

- **Route detail layout sync** (commit `deb4389`) — mirrors
  what slice 30 did for group detail. Metadata moves into
  the page header `<dl>` next to H1 + the hit-count
  subtitle, no more standalone Metadata card. The slice-26
  "Manage" card retires; its actions move into a
  `.page-footer` row: **Route state** link · **Run dry-run**
  placeholder (still CLI-only, rendered muted with a
  tooltip) · **Delete route** button. Breadcrumb drops the
  `#N` prefix and shows just `METHOD path`. CLI hint about
  `wm routes test` / `wm routes update` lives in a muted
  paragraph above the footer until source editing + the UI
  dry-run modal land.
- **Tokens page polish** (commit `b3d5ef3`) — TTL is now a
  preset dropdown (`Never` / `30 days` / `90 days` /
  `1 year` / `Custom`) with the old `ttl_hours` field
  kept alongside as the custom-hours input. The old form
  (no preset, just hours) keeps working. Column headers
  Name / Created / Last used / Expires are sortable anchor
  links with arrow on the active column. `expires` treats
  None as the largest value (immortal tokens sit at the
  end ascending, top descending); `last_used` uses default
  Option ordering. Four new tier-2 tests.
- **Token rename** (commit `43971fb`) — end-to-end feature
  add. `Auth::rename_token(owner_id, old, new)` preserves
  id + hash + metadata (plaintext keeps authenticating),
  swaps the `token:by-name:{owner}:{name}` index in
  write-new → update-record-name → drop-old order so a
  crash leaves a redundant entry rather than a dangling
  lookup. REST `PATCH /__api/tokens/{name}` with
  `{ "name": "new" }`; CSRF-gated UI form per row using a
  small `prompt()` for the new name. `NameTaken` → 409 /
  inline 400 on the UI; empty name rejected at the entry
  point. Four unit tests in `auth::tests` + three tier-2
  tests in `ui_tokens.rs`.
- **Journal entry layout sync** (commit `1ba67ca`) —
  breadcrumb now walks `Groups → group → route → #N` (was
  `Live journal → group → #N`). Status / Duration / Trace
  move into a `.meta-grid` `<dl>` in the page header; the
  separate Summary card retires (matched pattern folds
  into the description line, path_params + query render
  as a muted line under the dl, entry id drops, wall clock
  is now Duration). Dropped reserved headers collapse into
  a `<details>` inside Response. Handler errors promote
  to a `.card--error` callout above Request. Request /
  Response stay as split header-table + body-block
  sections rather than the wireframe's combined HTTP-style
  `<pre>` — documented as a deliberate divergence
  (binary bodies render cleanly).
- **Routes list group filter + state-page Back links**
  (commit `e2cec9d`) — small leftovers bundle. Routes
  list's Group filter switches from a free-text input to
  a `<select>` of the caller's non-implicit groups
  (admin sees all). Both `/__ui/groups/{group}/state`
  and `/__ui/routes/{group}/{n}/state` get a
  `← Back to {target}` link in a `.page-footer` row,
  pairing the destructive Clear button with a non-
  destructive sibling.

After slice 31 the spec catch-up commit walked the
wireframe doc and added "Slice 31 state" notes per
affected page, plus dropped the now-implemented
"deferred" markers (where the audit found us already
shipping the item). Still deferred at end of slice 31:
OAuth (login page), Source editing + Dry-run modal (route
detail), Pause/Resume on the Live journal,
"Did you mean…" suggestions on Unmatched (needs
`near_misses` populated upstream), Admin health + Settings
stub pages, CodeMirror across the source viewer/editor.

## Group detail + route-new wireframe sync (slice 30)

Cleanup slice driven by a dogfood pass: two pages had drifted
from their wireframes and the route-new form had no natural
entry point.

- **Group detail (`/__ui/groups/{group}`)**: was rendering as
  a sequence of cards (page-header card with H1, then a
  Metadata card, then a Routes card, then a Live activity
  card, then a Manage card with all actions at the bottom).
  Wireframe shape is denser: H1 + description + metadata dl
  + Refresh TTL / Edit TTL action row all *inside* the page
  header; Routes section gets the `+ Add route` button in its
  header; Live activity card stays where slice 24 put it
  (full-width below routes — the wireframe was updated to
  match); a `.page-footer` row at the bottom carries Full
  journal · Group state · Delete group. The standalone
  "Manage" card from slice 26 is gone — its actions
  redistributed into header + footer positions. The
  "Manage from CLI" hint panel from slice 23 is gone too;
  buttons + `wm --help` cover discoverability.
- **Route creation (`/__ui/routes/new`)**: slice 29 had
  reused `.filter-form` (a horizontal flex row designed for
  list-page filter bars), which put every form field in one
  cramped row. Restructured to a `.form-grid` 2-column
  label/input grid for the metadata four (Method, Path,
  Group, Language) and a separate card for "Handler source"
  with the textarea full-width, footer with Create + Cancel —
  matching the wireframe directly.
- **Discoverability gap**: the form was reachable only via the
  unmatched-page deep link and direct URL. Slice 30 added:
  "+ Add route" button on group detail's Routes section
  header, pre-filling `?group={name}`; "+ New route" button
  next to the Routes list page's H1; and group-detail's
  empty-state copy linking to the form directly.
- **CodeMirror**: still deferred (still a plain `<textarea>`).
  The handler-source card explicitly labels it "plain
  textarea · CodeMirror later" so the gap is visible.
- **CSS additions**: `.form-grid`, `.source-editor`,
  `.page-footer`, `.page-header__row` (vertical alignment for
  a title with a button on the right).
- **Test churn**: `tests/ui_detail_pages.rs::group_detail_renders_metadata_and_routes_for_owner`
  switched from asserting on the gone "wm groups refresh"
  CLI hint to asserting on the new `+ Add route` link
  (`/__ui/routes/new?group=stripe-mock`).

## Web UI route creation form (slice 29)

Ninth UI slice. Replaces the `/__ui/routes/new` stub with a
working source-based create-route flow, sharing the validate-
compile-register pipeline with `POST /__api/routes`.

- **GET** renders the form (method dropdown, path input,
  group dropdown of the caller's owned groups + an "(new
  implicit group)" option that maps to `group: None`, language
  dropdown, source textarea). Honours `?method=&path=&group=`
  prefill — the unmatched page's "Create route from request"
  deep-link now lands on a populated form.
- **POST** is form-encoded (`_csrf`, `method`, `path`, `group`,
  `language`, `source`). The handler builds a
  `CreateRouteBody` and calls the new
  `pub(crate) api::create_route_core(state, auth, body)`
  helper extracted from the REST `create_route` handler. On
  success: 303 to `/__ui/routes/{group}/{number}`. On error:
  re-render the form with `error.title` / `message` /
  `diagnostics` from the `ApiError`, status 400, submitted
  values preserved.
- **Shared core**: `create_route_core` does the whole
  pipeline — reserved-path check, source/wasm exclusivity
  check, language branch (compile via sidecar for source,
  base64-decode + bindings-version check for `wasm`),
  `Component::from_binary` validation, registry insert,
  `RouteTable::refresh_after_create`. Two new accessors on
  `ApiError` (`code()`, `diagnostics()`) let the UI map
  REST error codes to UI error titles.
- **Form scope**: source-only — `typescript` or `javascript`
  (both supported by the sidecar). Pre-compiled wasm uploads
  remain a REST-only path; a textarea is the wrong UI for
  bytes-with-base64.
- **Implicit groups**: an empty group field maps to
  `group: None`, which the registry handles by creating an
  implicit single-route group. Implicit groups are filtered
  out of the dropdown — only named groups show up.
- **CSRF**: middleware handles it; `_csrf` is read off
  `tokio::task_local!` like every other authed POST.
- **Tests:** `tests/ui_route_new.rs` — 6 tier-2 tests: GET
  defaults / GET prefill / POST happy path against a mock
  compiler returning the echo fixture (verifies redirect +
  mock traffic actually hits the new route) / reserved-path
  → 400 inline error / no-compiler-configured →
  `compile_failed` surfaced / missing-CSRF → 403. The mock-
  compiler harness mirrors `tests/api_routes.rs`.
- **Not in this slice:** CodeMirror (still a `<textarea>`),
  syntax-highlighted compile errors with line numbers, file
  upload for wasm, the "+ Add route" button on group detail
  (would just link to this form pre-filled with `group=`).

## Web UI unmatched pages (slice 28)

Eighth UI slice. Promotes the `/__ui/unmatched` stub into the
admin-only unmatched-request view and adds a per-entry detail
page at `/__ui/unmatched/{number}`.

- **List page** (`GET /__ui/unmatched`):
  - Filter form: `method` (dropdown of canonical HTTP verbs +
    "Any") and `path_pattern` (free-form text, glob-matched
    against the request path).
  - Cursor pagination via `?before=N` — the page reads up to
    `limit+1` newest entries (or 200 when filters are active so
    narrow filters still tend to fill a page), filters
    in-process, and emits an "Older →" link carrying
    `before=<lowest-number-on-page>` when more remain. There's
    no "newer" link — the list is naturally tail-heavy and a
    user wanting the freshest entries clicks the page header.
  - 25 rows per page.
  - Each row: timestamp (`HH:MM:SS` + ISO `datetime` attr),
    `METHOD path`, entry number, "View request" link to the
    detail page, "Create route from request" link to
    `/__ui/routes/new?method=…&path=…` (target still stubbed).
- **Detail page** (`GET /__ui/unmatched/{number}`):
  request envelope only — no response (unmatched requests
  never reached a handler). Summary card with entry ID + trace
  ID, request card with headers table and body block (UTF-8
  with truncation note, or `(binary, N bytes)`). The
  "Create route from this request" button is the same deep-
  link as the list rows.
- **Filter agreement**: composes a `JournalFilter` with just
  `method` + `path_pattern` and runs
  `matches_unmatched(record)`, matching the REST
  `/__api/unmatched` surface exactly. Validation uses
  `api_filters::validate_method`; an invalid method (lowercase,
  bad chars) renders the standard 400 placeholder.
- **Authorization**: admin-only on both pages (host-wide
  view). Non-admin sees the same `forbidden_page` placeholder
  used elsewhere.
- **Tests:** `tests/ui_unmatched_pages.rs` — 10 tier-2 tests:
  empty state, list-after-traffic, method filter narrows,
  path-pattern glob narrows, bad method → 400, "Older →"
  cursor link appears past one page, 403 non-admin on the
  index, detail body renders (request body included), detail
  404 unknown, 403 non-admin on detail.
- **HTML escaping note:** minijinja escapes `/` to `&#x2f;`
  inside text content (it's not in the default-allowed
  charset). Tests that look at rendered paths must accept the
  escape form or split on the trailing component
  (`missing-thing`).
- **Not in this slice:** "Did you mean…" near-miss
  suggestions (the dispatcher writes `near_misses: vec![]`
  today — would need a Levenshtein-distance lookup at
  journal-write or read time), and the actual
  `/__ui/routes/new` form the deep-links point at — that's a
  later slice. The `ui_smoke.rs` placeholder-routes loop was
  trimmed to just `/__ui/settings` to reflect the new state.

## Web UI state pages (slice 27)

Seventh UI slice. Promotes the `/__ui/routes/{group}/{n}/state`
and `/__ui/groups/{group}/state` stubs into real pages — a
read-only window onto the route's private `kv:` namespace and
the group's shared `gkv:` namespace, with a "Clear state"
button for either.

- **Route state page** (`GET /__ui/routes/{group}/{n}/state`):
  pulls entries via `Registry::list_route_state(group_id,
  route_id)`. POST to the same URL clears (calls
  `Registry::clear_route_state`) and 303-redirects back.
- **Group state page** (`GET /__ui/groups/{group}/state`):
  new `Registry::list_group_state(group_id)` mirrors
  `list_route_state` but reads from `Storage::group_bucket`
  (the `gkv:{group_id}:` namespace). POST clears via
  `Registry::clear_group_state`, which wipes **both** `kv:`
  and `gkv:` prefixes for the group — same semantics as
  `cascade_delete_group` minus the registry-record deletion.
- **Authorization**: standard owner-or-admin via
  `resolve_owned_group`. Non-owner → 403, unknown group/route
  → 404. CSRF on every POST.
- **Entry rendering**: `StateEntryView::from(&RouteStateEntry)`
  decodes `Bytes` as UTF-8 when valid (shown as `<code>` with
  byte-size annotation), otherwise reports `binary, N bytes`.
  Lists / sets / hashes show their length only. Pluralisation
  via minijinja `{% if %}` against `data.total` /
  `e.length`.
- **Navigation**: an "Inspect state" `.btn--ghost` lives in
  each detail page's `.action-row` next to the destructive
  actions. Breadcrumbs from the state page go: Groups → group
  → (route → )State.
- **Tests:** `tests/ui_state_pages.rs` — 9 tier-2 tests:
  empty state on both pages, list-after-dispatch (drives
  the `counter_handler` fixture and checks the entries
  table renders), clear-state form (redirects + leaves
  "No state yet"), 403 non-owner, 404 unknown, plus a
  group-clear-also-wipes-route-state check that verifies
  the shared deletion semantics.
- **Not in this slice:** value editing, key-by-key deletion,
  per-entry inspect-bigger-blob drilldown — kv is meant to
  be inspected, not edited, from the UI. Wiping is the only
  mutation.

## UI 404 page (slice 26 polish)

A typo under `/__ui/*` used to fall through to the dispatcher's
generic `not_found_response`, returning JSON `{"error":{"code":
"not_found", ...}}` — fine for `/__api/*` consumers but ugly for
a human in a browser. Now `dispatch_inner`'s reserved-path
branch detects the `/__ui/` prefix specifically and calls
`ui::render_not_found(&state, path)` to render a branded
`not_found.html` extending `base.html`.

The other 404 paths stay as-is — they're the design:
- `/__api/*` typos: JSON, machine-readable, no journal write.
- `/__auth/*` typos: JSON, same shape.
- Mock-traffic 404s: JSON + write to the unmatched journal so
  operators see what their SUT was hitting that they hadn't
  mocked yet.

The UI 404 handler doesn't check auth — a 404 reveals nothing
sensitive, and gating it behind the login flow just adds a
confusing redirect. The base layout's `{% if user %}` guard on
the user area handles the no-user case cleanly. minijinja's
auto-escape neutralises any HTML smuggled in the requested
path.

5 tier-2 tests in `tests/ui_not_found.rs` lock in: UI typo →
HTML 404 with the app shell, `/__api/typo` → JSON 404,
`/__auth/typo` → JSON 404, mock-traffic typo → JSON 404,
and a smoke check that `<script>` in the URL doesn't survive
into the rendered body.

## Web UI action buttons (slice 26)

Sixth UI slice. Replaces the "Manage from CLI" panels on the
group + route detail pages with real action buttons backed by
the CSRF wiring landed in slice 25.

- **Group actions** (`/__ui/groups/{group}/...`):
  - `POST .../refresh` — re-arms the Valkey TTL to the
    configured value via `Registry::refresh_group`.
  - `POST .../edit` — accepts `ttl_seconds` (positive integer)
    and `sliding_ttl` (HTML checkbox semantics: present + truthy
    = on; absent or empty = off). Calls
    `Registry::patch_group(ttl_seconds, sliding_ttl)`.
    Validation failure (zero, negative, non-numeric) renders the
    400 placeholder page.
  - `POST .../delete` — `Registry::cascade_delete_group` +
    `RouteTable::refresh_after_group_cascade`. Browsers see a
    `confirm()` prompt before the form submits.
- **Route action** (`/__ui/routes/{group}/{n}/delete`):
  `Registry::delete_route` + `RouteTable::refresh_after_delete`.
  Redirects back to the group detail if the group survived
  (explicit groups do; implicit single-route groups vanish), or
  the listing otherwise.
- **Authorization**: every action handler runs
  `resolve_owned_group` (or its route-shaped sibling) which
  returns `Box<Response>` for rejection paths — kept boxed so
  the `Result`'s `Err` variant doesn't trip clippy's
  `result_large_err`. Same rule as the REST surface: 403 for
  non-admin-non-owner, 404 for unknown.
- **CSRF**: every POST goes through the slice-25 middleware.
  Forms include `_csrf` from the request-scoped task-local; the
  `confirm()` prompts are pure UX with no security role.
- **CSS additions:** `.btn--danger` (red outline → solid on
  hover), `.action-row` (flex-wrap row of buttons),
  `.edit-disclosure` (clickable `<summary>` accenting + open-state
  spacing), `.filter-checkbox` (inline label + checkbox).
- **Tests:** `tests/ui_actions.rs` — 8 tier-2 tests covering
  refresh-redirects, edit-persists, edit-validation-error,
  delete-cascade, route-delete, non-admin 403s on both
  endpoints, and the CSRF-missing 403 path.
- **Not in this slice:** the "+ Add route" button on group
  detail (waits for the route creation form), dry-run modal,
  source editing.

## Web UI tokens page + CSRF middleware (slice 25)

Fifth UI slice. Replaces the `/__ui/me/tokens` stub with a real
self-service tokens page (list / create / revoke), and lands the
double-submit CSRF middleware that every future authed UI form
will rely on.

- **CSRF middleware (`ui::csrf::csrf_middleware`):**
  - Safe methods (GET/HEAD/OPTIONS): if no `wm_csrf` cookie was
    sent, mint one; either way, stash the active token in a
    `tokio::task_local!` (`CURRENT_CSRF`) for the duration of the
    request, and `Set-Cookie` on the way out if we minted.
  - Mutating methods (POST/PUT/PATCH/DELETE): reject 403 if no
    `wm_csrf` cookie was sent. Buffer the request body (cap 64
    KiB), parse it as urlencoded, pull out `_csrf`, compare to
    the cookie. On mismatch → 403. On match → rebuild request
    with the same body bytes so the downstream handler can still
    extract its own `Form<>`.
  - Cookie attributes: `HttpOnly; SameSite=Strict; Max-Age=86400`.
    `SameSite=Strict` is the real defense — cross-site requests
    don't carry the cookie at all, so they fail the
    cookie-presence check before any body comparison runs. The
    form field is the secondary check.
  - Applied to `/__ui/*` (in `ui::router`) and `/__auth/*` (in
    `auth_api::router`) as a `Router::layer` of
    `middleware::from_fn`.
- **Token injection into templates:** `ui::render` reads the
  task-local CSRF value and merges `csrf_token` into the
  caller's context using minijinja's `context!{..inner}` spread
  syntax. That's the only viable path — `context!` produces an
  opaque tuple-struct shape that serde's `#[serde(flatten)]`
  can't merge through (try it and you'll see "can only flatten
  structs and maps (got a tuple struct)"). With the spread,
  handlers stay focused on page data; `base.html`'s logout form
  and every form template get `{{ csrf_token }}` for free.
- **Tokens page (`/__ui/me/tokens`):**
  - GET: lists the user's own tokens (`auth.list_tokens_for`),
    sorted newest first. Empty state hints at the create form.
  - POST: validates name + optional TTL hours, calls
    `auth.create_token`, renders the same page with the
    plaintext token shown in a "you'll only see this once" card.
    Refreshing the page (or any subsequent GET) hides the
    plaintext — it lives only in that one response body.
    Validation errors render inline with a 400 status.
  - POST `/:name/revoke`: calls `auth.revoke_token_by_name` then
    303s back to the list. Idempotent — revoking a missing name
    is a no-op (race with another tab).
  - Admins managing other users' tokens still go through the CLI;
    the page is "your own tokens only" by deliberate design.
- **Test plumbing:** existing tier-2 tests' `login_cookie`
  helpers had to learn the CSRF dance — GET `/__auth/login` to
  mint the wm_csrf cookie and read the embedded `_csrf` form
  value, then POST with both. The combined cookie string
  `wm_csrf=X; wm_session=Y` is what subsequent requests send.
  Helper updated across `ui_smoke`, `ui_list_pages`,
  `ui_detail_pages`, `ui_journal_pages`, `local_auth_e2e`.
- **New tier-2 file**: `tests/ui_tokens.rs` — 8 tests covering
  empty state, create flow (plaintext-once), revoke flow,
  validation errors, three CSRF rejection paths (missing form
  field, mismatched form field, missing cookie), and ownership
  scoping (alice doesn't see admin's tokens).
- **Not in this slice:** route creation form, group/route state
  pages, dry-run modal — they'll reuse the same CSRF wiring when
  they land. Token plaintext display also waits for a clipboard
  copy-button (a polish item).

## Web UI live journal + journal entry (slice 24)

Fourth UI slice. Replaces the slice-21 stubs at
`/__ui/journal/live` and `/__ui/journal/{group}/{n}` with real
pages backed by the slice-11 SSE tail and journal record store.

- **Live journal (`/__ui/journal/live`):**
  - Pre-renders ~25 most-recent entries server-side when scoped
    to a group (via `journal.list_for_group` + in-process method/
    status/path_pattern filter — mirrors the SSE filter shape).
  - Opens an `EventSource` against `GET /__api/journal/tail` and
    prepends a row for each `event: handled`. Plain JavaScript
    (~50 LoC inline in the template) — HTMX comes in when there's
    a screen that wants HTML-fragment swaps from the server.
  - Filter form (group, method, path pattern, status) is a plain
    `<form method="get">`. The selected filters travel into the
    SSE URL via `build_sse_url` so the page and the tail are
    always in sync.
  - Authorization: with `?group=`, owner-or-admin (same rule as
    `tail_journal`); without, admin-only (host-wide tail).
    Non-admin without `?group=` lands on a picker-only view that
    doesn't open an SSE connection. Unknown group → 404,
    non-owner with `?group=` → 403.
  - Auto-cap: client keeps at most 200 rows in the DOM to bound
    memory if the page is left running. Reconnect on SSE error
    is handled by the browser's EventSource automatic retry.
- **Journal entry (`/__ui/journal/{group}/{n}`):**
  - Reads `journal.get(group_id, number)`. Owner-or-admin gate.
  - Renders request envelope (method, path, query, headers,
    body), response envelope (status, headers, body), handler
    logs, timing, trace ID. Text bodies decode as UTF-8 if
    clean; control bytes flip them to `(binary, N bytes)`.
    Body-truncation warnings show the original size when the
    journal trimmed it.
  - Breadcrumb back to the live journal preserves the group
    filter; explicit link to the matched route's detail page.
- **`tojson` filter:** minijinja gains the `json` feature so
  templates can embed the SSE URL in inline JS via `{{ url |
  tojson }}` (string properly quoted + JS-escaped). Cheap; pulls
  serde_json which is already in the workspace.
- **CSS:** no new classes — `.status status-2xx..5xx`,
  `.meta-grid`, `.code-block` were all already in place.
- **Tests:** `tests/ui_journal_pages.rs` — 10 tier-2 tests
  covering page render with/without group, SSE URL preservation
  through filter changes, picker-only flow for non-admin without
  group, 403/404 paths, the journal entry detail with full
  envelope, and admin-can-view-any.
- **Not in this slice:** group-detail right-column SSE pane (a
  small follow-up now that the EventSource pattern is in place;
  same machinery scoped to a group via `?group=`), HTMX (waits
  for the first server-fragment-swap use case), source viewer
  on the journal entry (would need handler-side rich-text
  formatting; deferred).

## Web UI detail pages + `/` redirect (slice 23)

Third UI slice. Replaces the slice-21 stubs at
`/__ui/groups/{group}` and `/__ui/routes/{group}/{n}` with real
detail pages, and implements the bare-`/` redirect from
`route-model.md`.

- **Group detail (`/__ui/groups/{group}`):** breadcrumb back to
  the listing, metadata grid (TTL, sliding flag, last activity,
  created, group ID), routes-in-group table with link-through to
  per-route detail, and a "Manage from CLI" panel listing
  `wm groups refresh / update / delete` plus `wm journal tail`.
  Source of truth: `registry.read_group_by_ref` + `registry.list_routes`
  filtered in-process.
- **Route detail (`/__ui/routes/{group}/{n}`):** breadcrumb,
  metadata (methods, path, language, bindings, component size,
  owner, route ID, created), recent journal entries
  (`journal.list_for_group` → filtered by `route_id`, capped at 10),
  and a "Manage from CLI" panel listing `wm routes test / state /
  delete`. Source viewing waits for the CodeMirror slice — the
  page surfaces "component size: N KiB" and stops short of
  showing handler source (the registry stores compiled wasm, not
  TS source, so source rendering needs registry changes too).
- **Authorization model:**
  - Unknown group / route → **404** via `ui_not_found` (placeholder
    template with `Not found` heading and `status=404`).
  - Non-admin viewing someone else's group / route → **403** via
    `forbidden_page` (same template, admin-role-required hint).
  - Admin can view anything. Same rule as the REST surface
    (`ensure_group_owner_or_admin`).
- **`GET /` redirect:** in `server::dispatch_inner`, when no user
  route matches and method+path are exactly `GET /`, the host
  returns `Redirect::to("/__ui/")` if a valid session cookie is
  attached or `/__auth/login` otherwise. A user-registered `GET /`
  route shadows the redirect because `find_match` returns `Some`
  before the fallback executes. The redirect deliberately does
  NOT write to the unmatched journal — a browser pointed at the
  bare hostname isn't a missing-mock signal.
- **Session check in dispatch:** the `has_valid_session` helper +
  `pick_session_cookie` in `server.rs` duplicate the cookie-parse
  logic from `ui::auth_redirect` and `auth_api`. Three call sites
  of ~10 LoC each — within the "three is fine" threshold from
  CLAUDE.md; extracting a shared helper waits until a fourth
  consumer wants it.
- **CSS additions:** `.breadcrumb` / `.breadcrumb__sep`,
  `.meta-grid` (auto + 1fr two-column DL), `.status` pill. Status
  colour classes (`status-2xx` etc) were already in place from
  earlier slices.
- **Templates:** `group_detail.html`, `route_detail.html`. Both
  extend `base.html`. Authed-action affordances appear as plain
  text in the CLI hint section — no disabled-button mockery, no
  half-implemented forms.
- **Tests:** `tests/ui_detail_pages.rs` — 11 tier-2 tests covering
  metadata render, 404 / 403 paths, admin-can-view-any, the
  detail page's recent-journal block reflecting traffic, both
  `/` redirect destinations, and the user-`GET /`-shadows-redirect
  guarantee.
- **Not in this slice:** authed actions (refresh / edit / delete /
  dry-run buttons — need CSRF), source viewer (needs source
  storage on registry + CodeMirror), HTMX-driven live updates
  (separate slice).

## Web UI list pages (slice 22)

Second slice of the UI track. Replaces the slice-21 stubs at
`/__ui/groups` and `/__ui/routes` with real list pages on top of
the slice-18 filter/sort/paginate surface.

- **Shared core extracted in `api.rs`:** `list_routes_core` /
  `list_groups_core` (both `pub(crate)`) hold the
  filter→sort→paginate path. The REST handlers and the new UI
  handlers both call them — no logic duplication. Each returns a
  `PagedRoutes` / `PagedGroups` struct (`Vec<Route|Group>` + total
  + next_offset). The `RoutesListQuery` / `GroupsListQuery`
  structs gained `pub(crate)` on every field so the UI can build
  one from a different input shape and read the active sort for
  arrow rendering.
- **`AppState` extended with no new fields** — the UI handlers
  reach through `state.routes()` / `state.auth()` just like the
  REST handlers do. Owner names resolve via
  `state.auth().get_user_by_id(...)`, batched per page in
  `resolve_owner_names`.
- **UI query shape vs. API query shape:** UI input uses
  `owner_scope=mine|everyone` (admin-only on the form); non-admins
  never get the field rendered and the field on the URL is ignored.
  The handler maps `owner_scope` → `owner_id` before calling the
  core fn, which already enforces "non-admin may not pass
  `owner_id`". An attacker handing the URL `?owner_id=…` directly
  hits a serde struct without that field — quietly ignored.
- **Sort toggles:** column headers are links to the same page with
  `?sort=X&dir=Y` filled in. Clicking the active column flips the
  direction; clicking a different column resets to that column's
  default direction (`name` defaults asc, time/count columns
  default desc). Arrow indicators (`↑`/`↓`) ride on the active
  column.
- **Pagination:** `UI_PAGE_LIMIT = 25`. The host's `parse_pagination`
  rejects `limit > 200`, so the constant lives in UI code rather
  than being toggleable per request. Prev/next links rebuild the
  URL with the current filters preserved (via
  `serialize_for_paging`) and the new `offset=`.
- **Bad-filter error path:** `ui_error_400` renders the same
  placeholder template with `page_title: "Bad filter"` and the
  `ApiError.message()` as the hint, and sets the status to 400.
  Adds `pub(crate) fn message(&self) -> &str` on `ApiError` so the
  UI doesn't need to format-debug its way to the human-readable
  copy.
- **New CSS classes:** `filter-form` (flex-wrap row), `filter-field`
  (column with `--label` span + input), `btn--ghost` (subtler
  border), `btn--disabled` (greyed-out, pointer-events: none),
  `pagination` (space-between layout).
- **Templates:** `groups_list.html` + `routes_list.html`, both
  extending `base.html`. Filter form is a plain `<form
  method="get">` — the browser handles serialization, so no
  JavaScript and no HTMX involvement yet.
- **Defaults:** admins land on "everyone" (the page is the catalogue
  view, the home page already covers "just yours"); non-admins are
  silently pinned to themselves. Default sorts: groups by
  `last_activity_at desc`, routes by `last_hit_at desc`.
- **Tests:** `tests/ui_list_pages.rs` — 10 tier-2 tests covering
  render, owner scoping (both directions), filter echo, sort
  toggle, pagination, 400 path, and the "raw owner_id sneaks in"
  case.
- **New dep:** `urlencoding = "2"` for query-string building.
  ~50 LOC of safe Rust with no other deps; cheaper than pulling
  `percent-encoding` directly with our own encode-set.
- **Not in this slice:** detail pages (Group/Route), creation
  forms, HTMX. CSRF stays unwired until the first authed UI form
  (likely the create-route slice).

## Web UI foundation (slice 21)

First slice of the UI track. Ships the templating + asset pipeline,
implements the design-tokens CSS, replaces the slice-20 inline
login page with a templated one, and adds a real home page plus
stubs for every other `/__ui/*` route — so navigation works
end-to-end before the detail pages land.

- **Templating:** `minijinja` 2.x with features
  `builtins`, `multi_template`, `serde`. Templates are embedded via
  `include_str!` at compile time in `ui::UiTemplates::new()`; one
  `tmpl!` macro line per file keeps the call site short. minijinja
  auto-escapes HTML — including `/` to `&#x2f;` in attribute
  values — which is correct but means tests that grep for raw
  paths in rendered HTML need to match unescaped substrings.
- **`crates/wm-host/src/ui/`:**
  - `mod.rs` — sub-router, handler functions, `UiTemplates`
    wrapper, `UserBadge` helper, `render` / `ui_error_500` /
    `stub` / `forbidden_page` glue. `ui::router(state)` returns a
    fully-stateful `Router` (state passed in so the auth-redirect
    middleware can hold a clone — same shape as
    `mcp::router(state)`).
  - `auth_redirect.rs` — middleware on `/__ui/*` (except
    `/__ui/static/*`). Reads the `wm_session` cookie via
    `session.touch()`; on failure issues `302` (axum's
    `Redirect::to`, actually 303 by default) to
    `/__auth/login?next=<original_path>` with the path
    percent-encoded.
  - `static_assets.rs` — `/__ui/static/{*path}` serves CSS/JS
    from `include_bytes!`. One file today (`wm.css`); HTMX lands
    alongside the slice that needs it. `Cache-Control: no-store`
    until we have content-hashed filenames.
  - `csrf.rs` — token mint helper. **Not wired up in this slice**
    — the only authed UI form is logout, which is a guarded
    no-arg POST; first authed form-with-data lands in slice 25.
    Module exists so the slice that needs it has somewhere to
    grow.
  - `templates/` — `base.html` (layout + nav), `login.html`
    (renders OAuth-buttons-shaped slot too), `home.html`,
    `placeholder.html`. Each page extends `base.html`.
  - `static/wm.css` — full implementation of `web-ui-design.md`'s
    "Visual design" section: tokens (light + dark via
    `prefers-color-scheme`), 14px-base type scale, 4px spacing
    grid, card / data-table / btn / badge / status-2xx..5xx
    classes, plus the auth-card layout for the login page.
- **`AppState` extended** with `ui_templates: UiTemplates` plus a
  read accessor. The Environment is built once at startup.
- **`server::router`** now `.merge(ui)` after `.merge(mcp)`.
- **`auth_api::login_page`** swapped from two `r#"..."#` const
  pages to a single `login.html` render with `local_enabled`,
  `next`, `error` in the context. Honours `?next=` on GET so the
  hidden form input round-trips through the redirect flow.
- **`just run-web`** convenience target: brings up the realistic
  stack — Valkey + TypeScript sidecar via `docker compose up -d`,
  waits for both to be reachable, then runs the host locally with
  `WM_STORAGE=redis://localhost:6379`,
  `WM_COMPILER_URL=http://localhost:9100`,
  `WM_LOCAL_AUTH='admin:devpassword:admin,user:devpassword'`, fixed
  `SESSION_SECRET`, then `cargo run -p wm-host`. Data persists
  across host restarts in the Valkey volume (`docker compose down -v`
  to wipe). Visit `http://localhost:8080/__ui/` and log in.
  `just run-web-fast` is the in-memory, no-sidecar shortcut for
  when you don't need persistence or TS compilation.
- **Stubs:** `placeholder.html` is shared by every "coming in a
  later slice" route. The stub handler names the equivalent API
  path so the user can drop to `wm`/`curl` until the real page
  ships. Admin-only stubs (`unmatched`, `settings`,
  `admin/health`) return 403 for non-admin via the same template.
- **Tests:** `tests/ui_smoke.rs` — 10 HTTP tests covering the
  full browser-driven flow: unauth → redirect, login page
  renders, `next` round-trip, post-login home, admin badge, CSS
  served, placeholder copy + API hint, admin-only 403, logout
  loop.
- **Not in this slice:** HTMX (lands alongside the live-journal
  slice), CSRF middleware (lands alongside the first authed UI
  form), OAuth provider buttons (slice 27).

## Local auth + browser sessions (slice 20)

Implements ADR-0018 (local username/password accounts via env var)
plus the shared session-cookie machinery the web UI and OAuth (when
it lands) will also use. Both pieces ship together — local auth's
whole purpose is to mint a session, and a session without a login
method to mint it isn't useful.

- **`crates/wm-host/src/local_auth.rs`** — parses
  `WM_LOCAL_AUTH=alice:hunter2:admin,bob:pw,...` into a
  `LocalAuth { username → { argon2id_hash, role } }` map. Fail-fast
  on duplicate names, empty fields, or an unknown role. `verify()`
  returns `Invalid` for both wrong-password and unknown-user — we
  don't distinguish, by design. argon2id chosen over bcrypt per
  OWASP guidance; ADR-0018's hashing-note section records the
  reasoning. PHC-encoded so we can bump cost factors later without
  breaking existing hashes.
- **`crates/wm-host/src/session.rs`** — `SessionStore` keyed by a
  signed HMAC-SHA256 cookie. Cookie wire format:
  `{32-byte-random-b64}.{HMAC-b64}`; rotating `SESSION_SECRET`
  invalidates every existing cookie. `Session` records persist at
  `session:{token}` with the fields from auth-and-authz.md
  (user_id, provider, created_at, last_seen_at, expires_at,
  ip_first_seen, user_agent). Sliding 24h TTL; both an in-record
  `expires_at` and a Valkey TTL on the key. `touch()` verifies the
  HMAC *and* the expiry before bumping the timestamps — initial
  implementation skipped the signature check on touch, the
  tampered-cookie test caught it.
- **`crates/wm-host/src/login_throttle.rs`** — per-IP throttle
  (5 fails / 60s window → 60s lockout). In-process; resets on
  successful login. Sliding window: a stale window auto-resets on
  next failure. Keyed by IP from `X-Forwarded-For` first, falling
  back to `127.0.0.1` (we don't wire `ConnectInfo` into the
  server — would mean touching every test harness). For the threat
  model the fallback collision is fine.
- **`crates/wm-host/src/auth_api.rs`** — routes under `/__auth/*`:
  `GET /__auth/login` (HTML form; switches to a "no methods
  configured" body when local auth isn't set), `POST
  /__auth/login/password` (form-encoded; vague `401 login failed`
  on bad creds, `429` when throttled, `303 See Other` + Set-Cookie
  on success), `POST /__auth/logout` (204 + Max-Age=0 cookie
  clear; idempotent). Login validates the `next` redirect target
  is a host-relative path so a crafted login URL can't open-
  redirect the browser.
- **`AuthContext` extractor extended** — tries `Authorization:
  Bearer wmt_...` first, falls back to a `wm_session` cookie when
  no bearer token is present, returns 401 when both miss. A new
  `CredentialKind` enum field (`Token` | `Session`) on
  `AuthContext` lets future handlers branch on credential type;
  the existing `token_id` field was renamed to `credential_id`.
- **`AppState` extended** — gained `local_auth`, `sessions`
  (`Option`), and `login_throttle` fields with matching
  `with_local_auth`, `with_sessions` builders and read accessors.
- **`Auth::upsert_local_user`** — first-login flow. Creates a user
  if missing, or updates the existing `is_admin` flag to match the
  env-var role (per ADR-0018: "Admin role lives in the env var").
  Writes `user:by-identity:local:{username}` as the lookup index.
- **`main.rs` wiring** — `configure_local_auth` reads
  `WM_LOCAL_AUTH` + `SESSION_SECRET` after `AppState::new`. Both
  optional individually; setting `WM_LOCAL_AUTH` without
  `SESSION_SECRET` is fatal at startup (login can't mint cookies
  without a signing key). A loud warning logs whenever local auth
  is configured — testing/private use only.
- **Dependencies added:** `argon2 = "0.5"` (PHC-encoded
  hashing), `hmac = "0.12"`, `subtle = "2.6"` (constant-time
  signature compare).
- **Tests:** unit tests in each module (14 + 8 + 7) plus 11 tier-2
  HTTP tests in `tests/local_auth_e2e.rs` covering: cookie issued
  on valid login, vague 401 on bad credentials / unknown user,
  lockout after 5 fails, cookie authenticates an API endpoint,
  logout invalidates, tampered-signature rejected, bearer-token
  path still works alongside sessions, login-page disabled when
  no methods configured, env-var role syncs `is_admin` on each
  login.

## CLI + MCP list flags (slice 19)

Wraps slice 18 in the user-facing surfaces: the `wm` CLI list
commands gain filter/sort/pagination flags, MCP tools gain the same
arg fields, and `wm unmatched list` lands as a new top-level
command (admin-only).

- **CLI:**
  - `wm routes list` — `--group`, `--owner-id`, `--method`,
    `--path-pattern`, `--since`, `--until`, `--q`, `--sort`,
    `--dir`, `--offset`, `--limit`.
  - `wm groups list` — `--owner-id`, `--name-prefix`, `--q`,
    `--since`, `--until`, `--implicit`, `--sort`, `--dir`,
    `--offset`, `--limit`.
  - `wm journal list <group>` — keeps `--before` / `--limit`,
    adds `--route`, `--method`, `--path-pattern`, `--status`,
    `--since`, `--until`.
  - `wm unmatched list` (new) — admin-only, cursor + same filters
    (no `route`, no `status`). `wm unmatched show <n>` reads one
    entry.
- **Human output:** the list renderers now show `LAST_HIT` /
  `HITS` on routes and `LAST_ACTIVITY` / `IMPLICIT` on groups
  (replacing the slice-9 columns since they're more useful than
  `LANG` / `SLIDING`). Paginated output ends with
  `(showing K of N; --offset M for the next page)`. JSON output
  carries the response verbatim (already includes `total` /
  `next_offset`).
- **wm-core:** flags map 1:1 onto `ListGroupsParams`,
  `ListRoutesParams`, `ListJournalParams`, `ListUnmatchedParams`
  via small `*_list_params` helpers in `wm-cli::handlers`.
  `Client::get_unmatched_entry` is new (admin-only). `UnmatchedRecord`
  was already added in slice 18.
- **MCP:**
  - `list_routes` — args extended with `owner_id`, `method`,
    `path_pattern`, `since`, `until`, `q`, `sort`, `dir`, `offset`,
    `limit`; result carries `total` + `next_offset`. Existing
    `group` + `mine` kept; non-admin still pinned to self.
  - `list_groups` — args extended with the full set
    (`name_prefix`, `q`, `owner_id`, `since`, `until`, `implicit`,
    `sort`, `dir`, `offset`, `limit`); result carries `total` +
    `next_offset`.
  - `list_recent_unmatched` — args extended with `before` (cursor),
    `method`, `path_pattern`, `since`, `until`; result carries
    `next_before`.
  - All three call the shared filter primitives via
    `crate::api::{...}` and `crate::api_filters::{...}`. Filter
    parse failures map to `ErrorData` via the new
    `map_filter_error` helper in `mcp/error.rs` — the structured
    `data` payload carries `{ code, parameter }` mirroring the
    REST error shape.
- **Shared helpers refactor:** the route / group sort comparators
  + `parse_pagination` + `slice_for_page` in `api.rs` were
  promoted to `pub(crate)` so the MCP tools reuse them — same
  semantics on both surfaces (notably: never-hit routes sort to
  the bottom of an activity sort, regardless of direction).
- **Tests:** four new CLI smoke tests (`binary_smoke.rs`) cover
  `--name-prefix`, `--limit`/`--offset`, `wm unmatched list
  --path-pattern`, and the bad-sort exit. Three new MCP e2e tests
  (`mcp_e2e.rs`) cover the same shape over MCP. Tier-2 + unit
  coverage from slice 18 carries over unchanged.

## List filter / sort / pagination (slice 18)

Adds a shared filter + sort + offset-pagination vocabulary across the
four REST list endpoints (`GET /__api/routes`, `/__api/groups`,
`/__api/journal/{group}`, `/__api/unmatched`). Implementation is
in-memory at the handler layer — small data sets, cheap to filter
and sort each request.

- **Parsing module:** `crates/wm-host/src/api_filters.rs` owns
  `glob_match` (`*`-wildcards over a flat string), `parse_since`
  (RFC 3339 or duration suffix `30s`/`5m`/`1h`/`2d`), `SortDir`,
  `validate_method`, and `FilterParseError` (with a
  `parameter()` accessor naming the offending query field).
- **Routes endpoint:** `?group=`, `?owner_id=` (admin-only —
  non-admin caller gets 403 with `parameter=owner_id` diagnostic),
  `?method=`, `?path_pattern=` (glob over the defined path),
  `?since=` / `?until=` (against `last_hit_at`; routes with no hit
  are excluded when either is set), `?q=` (substring needle over
  path + methods), `?sort=created_at|last_hit_at|hits_total`,
  `?dir=asc|desc`, `?offset=`, `?limit=`. Response now
  `{ routes, total, next_offset }`.
- **Groups endpoint:** `?owner_id=` (same admin rule),
  `?name_prefix=`, `?q=`, `?since=` / `?until=` (against
  `last_activity_at`), `?implicit=true|false`,
  `?sort=created_at|name|last_activity_at`, `?dir=`, `?offset=`,
  `?limit=`. Response now `{ groups, total, next_offset }`.
- **Journal endpoint:** keeps cursor pagination (`?before=`,
  `?limit=`) and adds `?route=`, `?method=`, `?path_pattern=`
  (glob over `matched_pattern`), `?status=` (`2xx`/`3xx`/`4xx`/`5xx`
  or exact code), `?since=` / `?until=` (against `created_at`).
  Filter is applied **after** the cursor page is fetched, so
  `next_before` always reflects the oldest *raw* entry on the page
  — the caller can keep walking even when filters reject everything.
- **Unmatched endpoint:** cursor + `?method=`, `?path_pattern=`
  (glob over request path), `?since=` / `?until=`. Admin-only.
- **Shared `JournalFilter`:** extended with `since` / `until`;
  `matches_handled` / `matches_unmatched` made `pub` so api.rs can
  reuse them. Path-pattern matching switched from exact-match to
  `glob_match`; the unmatched path-pattern now matches against the
  request path rather than implicitly hiding unmatched events.
  This also affects the SSE tail endpoint — the rules are now the
  same everywhere.
- **Pagination defaults:** routes/groups → offset/limit with
  `DEFAULT_LIST_LIMIT = 50`, `MAX_LIST_LIMIT = 200`. `limit=0`
  → 400 with `parameter=limit`.
- **Error shape:** validation failures from filter parsing return
  `code: validation_failed`, with `diagnostics: ["parameter=<name>"]`
  so clients can pinpoint the bad field without scraping the
  message. The single exception is `owner_id` from a non-admin →
  `code: forbidden`, status 403.
- **wm-core models:** new `ListGroupsParams`, `ListRoutesParams`,
  `ListJournalParams`, `ListUnmatchedParams` (each with
  `to_query_string()` doing minimal percent-encoding) and matching
  `Client::list_*_with(params)` methods. `Client::list_unmatched`
  and `UnmatchedRecord` are new to wm-core. The existing
  `list_journal(group, before, limit)` and no-arg `list_groups` /
  `list_routes` stay as forwarders to the params-taking variants
  for backward compat. `ListGroupsResponse` / `ListRoutesResponse`
  gained `total` + `next_offset` with `#[serde(default)]` so a
  pre-slice-18 host that doesn't emit them still decodes.
- **CLI / MCP wiring:** *not in this slice* — lands as slice 19.
  For now `wm` only consumes the unfiltered no-arg path.
- **Tests:** unit tests in `api_filters` cover glob, since-parsing,
  sort-dir, method validation, parameter routing. Tier-2 tests in
  `api_routes.rs` exercise group/method filtering, glob, offset
  pagination, owner_id admin-only with parameter diagnostic,
  bad-sort diagnostic, name_prefix + sort-asc on groups, and
  method/status filtering on the journal + path_pattern on
  unmatched.

## Activity tracking (slice 17)

Per-record bookkeeping bumped on every matched dispatch. Drives the
"most recently active" default sort for list endpoints (which lands
as slice 18) but the fields themselves are useful immediately.

- **Fields added:** `Route.hits_total` (`u64`), `Route.last_hit_at`
  (`Option<DateTime<Utc>>`), `Group.last_activity_at` (same).
- **Dispatch-path call:** `registry.record_route_hit(group_id,
  route_id, now)` in `server::dispatch_inner`, after the journal
  write and alongside the sliding-TTL bump. Best-effort — a failure
  logs `tracing::warn` and doesn't change the SUT's response. Three
  storage operations per matched request: `HSET route:{id}
  last_hit_at`, `HINCRBY route:{id} hits_total`, `HSET group:{id}
  last_activity_at`.
- **Backward compat:** pre-slice-17 records won't have the fields.
  `decode_route` / `decode_group` use a new `utf8_opt` helper that
  returns `None` on missing-field rather than erroring; `hits_total`
  defaults to `0`, `last_hit_at` / `last_activity_at` default to
  `None`. First post-upgrade dispatch populates them.
- **Cleanup paths updated:** the field lists in `delete_route` and
  `cascade_delete_group` include the new field names so a deleted
  record leaves no stragglers in storage.
- **Surface:** `RouteResponse` / `GroupResponse` (REST),
  `RouteRecord` / `GroupRecord` (wm-core models + MCP tool types)
  all expose the new fields. JSON output uses
  `skip_serializing_if = "Option::is_none"` for the timestamp
  fields so never-hit records don't surface `"last_hit_at":null` —
  the field is just absent.
- **Tests:** registry unit test
  (`record_route_hit_bumps_counter_and_timestamps`) covers the
  fresh-route, first-hit, second-hit sequence. Tier-2
  `activity_fields_bump_on_dispatch` drives three real HTTP
  requests against a route and verifies the fields propagate
  through to `GET /__api/routes/{slug}` and `GET /__api/groups/{name}`.

## Per-route state + dry-run (slice 16)

The second half of the route-debug pair. Three new host endpoints,
three new MCP tools, and `wm routes state` / `wm routes test` on
the CLI.

- **State endpoints:** `GET /__api/routes/{group}/{n}/state`
  lists each kv key with its storage-level kind (`bytes` / `list` /
  `hash` / `set` / `other`); bytes inline the value, collections
  report `length`. `DELETE` wipes the route's private kv namespace
  via the existing `delete_with_prefix`. Both owner-or-admin.
- **Dry-run endpoint:** `POST /__api/routes/{group}/{n}/dry-run`
  with a synthetic request body. Lives in `crates/wm-host/src/dry_run.rs`
  to keep server.rs lean. Owner-or-admin.
- **Snapshot semantics:** the route's `kv:{group}:{route}:*` and
  the group's `gkv:{group}:*` are deep-copied (preserving type) to
  a `dryrun:{run_id}:` root via `Storage::copy_keys_with_prefix`
  (in-memory clones `MemValue`; Valkey uses the `COPY` command).
  The handler is instantiated with buckets whose prefix points at
  the shifted root, so reads + writes go to the snapshot. On
  completion, one `delete_with_prefix("dryrun:{run_id}:")` wipes
  the snapshot. Journal is **not** written.
- **Crash safety:** every key in the dry-run namespace gets a 60s
  `PEXPIRE` so a host crash mid-dry-run doesn't leave orphans
  forever. No-op for the in-memory backend (restart wipes
  everything).
- **Handler trap behavior:** wasm traps become part of the dry-run
  *response* (status 500, `error` field set, partial logs
  preserved). The HTTP outcome stays 200 — the agent asked "what
  would happen?", and the trap *is* the answer.
- **`Bucket::kind(key)` introspection** added so the state listing
  can label values without using only typed getters. Returns
  `bytes` / `list` / `hash` / `set` / `other` / `None` (missing).
- **CLI:** `wm routes state <slug>` (list / `--clear`) and `wm
  routes test <slug> --method POST [--path /foo] [--header X:Y]
  [--body STRING|@FILE] [--path-param k=v]`. The CLI defaults the
  request path to the route's own path (one extra GET to fetch the
  route record) so `wm routes test slug` works without extra
  typing.
- **MCP:** `show_route_state`, `clear_route_state`, `dry_run_route`
  — all in `mcp/tools/state.rs` (renamed from "State tools" to
  "State + dry-run tools"). Bytes values are base64-encoded on
  the wire to keep the schema clean for JSON consumers.

## Route update (slice 15)

`PATCH /__api/routes/{group}/{n}` mutates a route in place. The
matching `wm routes update <slug>` CLI subcommand and the
`update_route` MCP tool round it out — same shape as the
create-route surface, partial body.

- **Mutable fields:** `methods`, `path`, the artifact triple
  (`source` or `compiled_wasm` + `language` + `bindings_version`).
  `owner_id`, `number`, `id`, and the parent `group` are immutable.
- **Conflict re-validation:** path or method changes walk the route
  set again (excluding self) and reject if the new shape would
  collide. Same `routes_conflict` helper as `create_route`. Test
  coverage: `patch_route_rejects_path_conflict` in
  `crates/wm-host/tests/api_routes.rs`.
- **Cache eviction:** any successful update calls
  `RouteTable::refresh_after_update`, which replaces the in-memory
  record and drops the cached `Component`. The next request hitting
  the route triggers a fresh compile. Eviction is unconditional —
  even metadata-only updates evict, on the theory that one extra
  compile is cheaper than a stale-bytes bug.
- **Auth:** owner-or-admin, matching DELETE. Non-owner non-admin
  gets 403.
- **MCP stays wasm-only on the artifact**, matching `create_route`.
  Source-based updates go through REST or `wm routes update
  --source-file` (which forwards through the same compiler sidecar
  used by `wm routes add`).
- **CLI flag semantics:** `wm routes update <slug>` requires at
  least one of `--method`, `--path`, `--source-file`, or
  `--wasm-file`; an entirely empty patch is a usage error caught
  client-side before the host call.

## User CRUD CLI + completions (slice 14)

`wm users` covers the admin user-management surface that previously
required curl. CLI-only by design — `mcp-surface.md` explicitly
excludes user management from MCP because it's a setup operation
done before agents connect.

- **Subcommands:** `list`, `show NAME`, `me`, `create NAME [--admin]`,
  `update NAME --admin|--no-admin`, `delete NAME --force`. Wraps
  the existing `/__api/users` REST endpoints (slice 5b).
- **Auth model unchanged:** admin-only for cross-user actions; any
  authed user can `wm users me`. `wm users show NAME` works for
  same-name (self) calls without admin.
- **`wm users update`** today only flips `is_admin` — the host's
  PATCH endpoint accepts only that field. Rename / merge are
  separate ADRs and not on the CLI yet.

`wm completion <shell>` emits bash/zsh/fish/powershell scripts to
stdout via clap_complete. No host or token required. Pipe into the
shell's completion directory.

## Match probe (slice 13)

`GET /__api/match?method=&path=` answers "what would handle this
request?" with either the matching route or a list of near-misses.
Mirrored as `wm match METHOD PATH` in the CLI and `find_route` in
MCP.

- **Code lives:** `RouteTable::probe` in `route_table.rs` builds the
  result; `prefix_segment_diff` near it computes prefix near-misses.
  `MatchProbe::Hit` is boxed to keep the enum's variants
  size-balanced (clippy's `large_enum_variant`); same trick on
  `wm-core::MatchResponse`.
- **Reasons shipped:** `method_mismatch` (pattern matches, methods
  don't) and `prefix_match` (single-segment literal-prefix typo,
  e.g. `/v1/charge` vs `/v1/charges`).
  `path_shape_match_in_other_group` from the design spec is *not* a
  distinct reason — that case shows up as a `method_mismatch` whose
  `route` field names the foreign group, so consumers can branch on
  that themselves.
- **Auth:** any authenticated user (read-only diagnostic). Cross-
  group visibility is intentional — the probe shows route paths and
  methods regardless of ownership; only modifications stay
  owner-or-admin.
- **Near-miss cap:** 20 entries (`NEAR_MISS_LIMIT`). If you hit it,
  you almost certainly have bigger problems than the order.
- **Tests:** matcher unit tests in `route_table::tests` cover hit /
  method-mismatch / prefix-match / unrelated-path / both-reasons /
  param-segments / two-segment-diff. REST integration in
  `api_routes.rs` (`match_probe_*`); `wm-core::Client` round-trip
  in `wm-cli/tests/wm_core_against_host.rs`; CLI binary in
  `binary_smoke.rs`; MCP via rmcp client in `mcp_e2e.rs`. Each layer
  has 1–6 cases — enough to catch wire-shape and code-path
  regressions without overspecifying near-miss content.

## Live journal tail / streaming (slice 11)

Live tail uses an in-process broadcast bus inside `Journal`
(`tokio::sync::broadcast`, capacity 256). `record_handled` and
`record_unmatched` publish a `JournalEvent` after the Valkey write
succeeds; consumers subscribe via `Journal::subscribe()`.

- **Filter shape.** `crates/wm-host/src/journal_filter.rs` —
  conjunctive `JournalFilter` over `group` (name or ULID), `route`
  slug, `method`, `path_pattern` (exact match against the route's
  matched_pattern), and `status` ("2xx"/"3xx"/"4xx"/"5xx" or a
  specific code). Per-route filters implicitly hide unmatched
  events. Used by both the SSE handler and the MCP streaming tools
  so semantics stay aligned.
- **HTTP surface.** `GET /__api/journal/tail` returns an axum SSE
  stream wrapped around `BroadcastStream`. Each matching event
  emits `event: handled` or `event: unmatched`; `event: warning`
  surfaces lag if a consumer can't keep up. Heartbeat via
  axum's `KeepAlive::default()`. Auth: owner-or-admin when
  `?group=` is set, admin-only otherwise.
- **MCP surface.** `wait_for_request` (count + timeout) and
  `tail_journal` (max_entries + idle timeout). Both subscribe
  directly to the in-process bus — no need to round-trip via SSE
  since the MCP server runs inside the host. Single
  `CallToolResult` with the accumulated entries (request/response
  shape, not progressive notifications) — matches what rmcp does
  cleanly.
- **Multi-host gap.** The bus is in-process. Sibling hosts in a
  multi-host deployment won't see each other's events. Documented
  in the implementation-status blocks; revisit when multi-host is
  real (Valkey pub/sub fits behind the same `JournalEvent` shape).
- **Lag handling.** A slow subscriber that lags past the channel
  capacity loses events. The SSE endpoint surfaces this as a
  `warning` event and keeps going; the MCP tools count dropped
  events into a `dropped_events` field on the result and continue
  accumulating. Documented; agents that care about completeness
  should subscribe before triggering the SUT.
- **Tests.**
  - Tier 1 — `journal_filter::tests`: 8 cases covering parsing +
    filter matching including unmatched semantics.
  - Tier 2 — `crates/wm-host/tests/journal_tail_sse.rs`: 7 cases
    against the SSE endpoint via reqwest streaming. Covers auth
    gates, group/method filters, unmatched delivery, invalid
    filter parsing.
  - Tier 2 — `crates/wm-host/tests/mcp_e2e.rs` adds `wait_for_request`
    happy path + timeout + non-admin auth gate, plus
    `tail_journal` idle-timeout exit.

### Adding a new event-shaped tool

For tools that observe live journal traffic: subscribe via
`state.journal().subscribe()`, build a `JournalFilter` from
user-facing args (`build_filter` in `streaming.rs` is reusable),
gate auth via `ensure_streaming_authorized`, then loop with
`tokio::time::timeout(...)` over `rx.recv()`. Map `Lagged` errors
into a `dropped_events` counter on the result; map `Closed` into
"return what you have" rather than failing. Don't try to map back
to progressive notifications unless we hit a concrete need —
single-result is what rmcp handles best and the design doc agrees.

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
