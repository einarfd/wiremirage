# CLAUDE.md

Bootstrap notes for Claude Code working in this repository.

## What this is

WireMirage: an agent-native, multi-language mock server. Handlers are real
code (TypeScript first), compiled to Wasm components, executed inside a Rust
host (`wasmtime`). Per-route isolated KV state; groups as TTL-bounded
lifecycle units. Storage in Valkey (Redis wire protocol). See `README.md`.

**Status:** slices 1–12 landed. The WIT contract is live at
`wit/wiremirage.wit`, the host (`wm-host`) instantiates components
against it, storage is abstracted behind a `Storage` enum with both
in-memory and Valkey backends, and routes are stored in a `Registry` +
`RouteTable` keyed by `{group}/{n}` slugs per `route-model.md`. The
REST API at `/__api/routes` supports POST/GET/DELETE for both
pre-compiled wasm uploads and TypeScript source — the source path goes
through a separate Node sidecar at `compiler/typescript/`
(componentize-js + jco), reachable via `WM_COMPILER_URL`. The
`/__api/*` surface is gated by bearer-token auth (bootstrap via
`WM_BOOTSTRAP_TOKEN=wmt_...` on first startup); mock traffic to user
routes stays open by design. Public probes: `GET /__health`,
`GET /__ready`. Token CRUD lives at `/__api/tokens`; user CRUD lives
at `/__api/users` (admin-only for cross-user actions, plus
`GET /__api/users/me` for any authed caller). Routes carry `owner_id`
and DELETE checks owner-or-admin. Every dispatched mock request lands
in a per-group journal (`/__api/journal/{group}`); unmatched requests
land in `/__api/unmatched` (admin-only). Both default to a 1h TTL.
Groups are first-class lifecycle units (`/__api/groups`) with
configured TTL (default 24h, max 30d) and sliding-on-traffic by
default; cascade-delete wipes routes, kv/gkv state, and journal
entries together. A background sweeper reaps the children of any
group whose Valkey TTL has fired. The `wm` CLI (slice 9) wraps the
REST surface end-to-end: groups, routes, journal, tokens, plus the
public probes. Auth via `WM_TOKEN` / `--token`, host via `WM_HOST` /
`--host`. `--json` switches to machine-parseable output for scripts
and agents. The MCP server (slice 10) is part of `wm-host` and
mounts at `/__api/mcp` over the streamable-HTTP transport (rmcp).
15 tools now cover identity, discovery, group/route CRUD, group
state, plus the slice-11 streaming pair (`wait_for_request`,
`tail_journal`) backed by `GET /__api/journal/tail` SSE on the host
and a single-host broadcast bus inside `Journal`. Same bearer-token
auth throughout. Multi-host fan-out (Valkey pub/sub) and 4
host-blocked tools land in follow-up slices. The user-facing skill
(slice 12) ships at `skill/wiremirage/` (with a debug sub-skill at
`skill/wiremirage-debug/`) — `SKILL.md` + 3 ready-to-run scripts
teaching the CLI workflow.

## Where the design lives

The design is captured as docs and ADRs in a private Arkiv workspace named
`wiremirage` (search via `mcp__claude_ai_Arkiv__search_workspaces` to find
the ID). Read with the `mcp__claude_ai_Arkiv__*` tools.

**Treat Arkiv as read-only by default.** Don't write/edit/delete files in
the workspace unless the user has explicitly asked you to.

Key documents to load early when working on a task:

- `index.md` — entry point and document map
- `architecture-overview.md` — components, request flows, deployment
- `adrs/index.md` — list of decision records
- The specific design doc the task touches (e.g., `route-model.md`,
  `storage-model.md`, `script-api-wit.md`, `cli-design.md`)

## Repo layout

Cargo workspace with three crates under `crates/`:

- `wm-core` — shared types, REST client, auth
- `wm-host` — long-running Rust server (axum + wasmtime + Valkey).
  MCP service is a `mcp/` module here, mounted at `/__api/mcp`.
- `wm-cli` — `wm` CLI binary

Plus a Node-based compiler sidecar:

- `compiler/typescript/` — accepts TypeScript source over HTTP, returns
  componentized wasm bytes. Built as its own Docker image
  (`compiler/typescript/Dockerfile`); the host calls it when the user
  POSTs `language: "typescript"` to `/__api/routes`. **Not** Rust, not
  in the cargo workspace.

The WIT contract that handlers program against lives at `wit/wiremirage.wit`.
It is the verbatim mirror of `script-api-wit.md` in the Arkiv workspace; if
you need to change it, update the design doc first.

Wasm guest fixtures used by the host's tier-2 integration tests live at
`crates/wm-host/tests/fixtures/<name>/` as **standalone crates** (their own
`Cargo.toml` + `Cargo.lock`, excluded from the parent workspace). The host's
`build.rs` compiles them to `wasm32-unknown-unknown` and runs `wasm-tools
component new` on the result; the resulting paths are stamped into env vars
of the form `WM_FIXTURE_<name>_COMPONENT` for tests to read via `env!()`.

The product skill (shipped to *users* of WireMirage) lives at
`skill/wiremirage/` per ADR-0015 (with a debug sub-skill at
`skill/wiremirage-debug/`). The dev skill at `.claude/skills/wm-dev/`
is for *developing this repo* — not the same thing.

**The product skill is tightly coupled to the current CLI surface.**
Any time you add, rename, or remove a `wm` subcommand or flag — or
change the handler API, route shape, etc. — check `skill/wiremirage/`
(SKILL.md + scripts/*.sh) and update what's affected in the same
change. Same for `skill/wiremirage-debug/SKILL.md`. The skill goes
stale fast and "describe the current surface" is the only commitment
worth making.

## Common commands

Use `just` (see `justfile`):

- `just check` — fmt check + clippy `-D warnings` + tests (skips Docker tests)
- `just check-all` — like `check` plus tier-3 Valkey + sidecar tests
- `just fmt` — format
- `just test` — workspace tests only (no Docker)
- `just test-valkey` — tier-3 Valkey-backed tests, requires Docker
- `just test-sidecar` — tier-3 TS sidecar tests; builds the image first
- `just build-sidecar-image` — build the sidecar Docker image only
- `just build` — `cargo build --workspace`
- `just run-host` / `just run-cli <args>`

To run the host with sidecar (for TypeScript handlers):

```sh
docker compose up -d   # starts valkey + compiler-typescript
WM_BOOTSTRAP_TOKEN=wmt_dev_local \
  WM_STORAGE=redis://localhost:6379 \
  WM_COMPILER_URL=http://localhost:9100 \
  cargo run -p wm-host
```

Or in-memory + no compiler (pre-compiled `language: "wasm"` only):

```sh
WM_BOOTSTRAP_TOKEN=wmt_dev_local WM_STORAGE=memory cargo run -p wm-host
```

Register a TypeScript route and call it:

```sh
curl -X POST localhost:8080/__api/routes \
  -H 'authorization: Bearer wmt_dev_local' \
  -H content-type:application/json \
  -d '{
    "methods": ["POST"],
    "path": "/v1/charges",
    "language": "typescript",
    "source": "export function handle(req,_r,_g){return {status:200,headers:[],body:new TextEncoder().encode(\"hi from \"+req.method)};}"
  }'
# Mock traffic does not need an Authorization header.
curl -X POST localhost:8080/v1/charges -d '{}'
```

Env vars (no silent fallbacks; missing required → fail-fast):

- `WM_STORAGE` (required) — `memory`, `redis://...`, or `rediss://...`
- `WM_BOOTSTRAP_TOKEN` (required on first startup, optional on
  restarts once at least one user exists) — plaintext for the admin
  user named `bootstrap`. Treat like a credential.
- `WM_COMPILER_URL` (optional) — URL of the TypeScript sidecar. If
  unset, source-based POSTs return `compile_failed`; pre-compiled
  uploads still work.
- `OTEL_EXPORTER_OTLP_ENDPOINT` (optional) — URL of an OTLP/gRPC
  collector (e.g. `http://localhost:4317`). When unset, the host logs
  to stderr only; when set, spans are exported in addition.
  `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` are honored too
  (standard OTel SDK behavior).

## Required tooling

In addition to a stable Rust toolchain:

- `wasm32-unknown-unknown` target — `rustup target add wasm32-unknown-unknown`
- `wasm-tools` CLI — `cargo install wasm-tools` (used by `wm-host/build.rs`
  to componentize fixture guests; also handy for `wasm-tools component wit
  <component.wasm>` when investigating component-shape issues)
- `just` — `cargo install just`
- **Node 22+** — only needed for hacking on the compiler sidecar
  (`compiler/typescript/`) or running its `npm test`. Not required to
  build or run the host.
- **Docker** — required for the tier-3 testcontainers suites
  (`just test-valkey`, `just test-sidecar`).

## Conventions

- Latest stable Rust, edition 2024. No MSRV pin.
- Clippy is `-D warnings` in CI; fix lints rather than allowing them.
- Significant design decisions go in an ADR before implementation; ADRs
  live in Arkiv at `adrs/NNNN-slug.md` and follow the structure documented
  in `adrs/index.md`.
- License is Apache-2.0. New source files don't need a header (the LICENSE
  file at the repo root covers them).
- Don't add `_unused` renaming, "kept for backwards compat" shims, or other
  decorative scaffolding when refactoring — delete the dead code.

## Slash commands

- `/check` — runs the full check suite and reports
- `/new-adr` — scaffolds a new ADR in the Arkiv workspace following the
  established conventions

## Subagents

Prefer `Explore` for codebase searches, `Plan` for non-trivial design
work. The `claude-code-guide` agent handles questions about Claude Code
itself. There are no repo-tuned subagents yet.
