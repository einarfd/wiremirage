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
