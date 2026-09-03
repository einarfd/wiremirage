# Contributor and agent guide

Orientation for anyone — human or agent — working *on* WireMirage. For using
WireMirage, start at [README.md](README.md) and [docs/](docs/).

## What this is

An agent-native mock HTTP server. Handlers are TypeScript/JavaScript source,
transpiled in-host and executed as WebAssembly components inside a Rust host
(axum + wasmtime). Each route owns a private KV namespace; groups are
TTL-bounded lifecycle units that also define a routing namespace (one
subdomain per group). Storage is Valkey (Redis wire protocol) or in-memory.

## Repo layout

Cargo workspace, three crates:

- `wm-core` — shared types, REST client, group-spec format
- `wm-host` — the server. Modules worth knowing: `server.rs` (dispatch),
  `route_table.rs` + `pattern.rs` (matching), `registry.rs` (routes/groups),
  `runtime.rs` (wasmtime + sandbox limits), `journal.rs`, `api.rs` (REST),
  `mcp/` (MCP service, mounted at `/api/mcp`), `ui/` (minijinja templates),
  `auth.rs` / `oidc.rs` / `github_oauth.rs` / `session.rs`
- `wm-cli` — the `wm` binary
- `wm-transpile` — TS→JS via swc. Its own crate because `wm-host`'s
  `build.rs` needs it too: `engine.ts` goes through the same transpiler as
  user handlers (ADR-0038). `transpile` returns script shape (handlers),
  `transpile_module` keeps the ES export (the engine build)

Outside the workspace:

- `compiler/js-engine/` — TypeScript shim + Dockerfile producing the shared
  `js-engine.wasm` (componentize-js over StarlingMonkey). Built at cargo build
  time by `crates/wm-host/build.rs` into `OUT_DIR` and `include_bytes!`'d into
  the host. Not vendored, not in the workspace. `build.rs` transpiles
  `engine.ts` with `wm-transpile` and passes the JS in; the container
  componentizes it and runs `tsc --noEmit` as a *checker* (ADR-0038 —
  TypeScript 7's programmatic emit API is gone, its CLI is fine).
- `types/wiremirage-handler.d.ts` — the handler contract as TypeScript, shipped
  for handler authors. Hand-written (the handler surface is a JS-ergonomics
  layer over the WIT), so `tests/handler_types_track_wit.rs` fails if it and
  the WIT disagree in either direction.
- `crates/wm-host/tests/fixtures/<name>/` — wasm guest fixtures as standalone
  crates. `build.rs` compiles them to `wasm32-unknown-unknown`, runs
  `wasm-tools component new`, and stamps the paths into
  `WM_FIXTURE_<NAME>_COMPONENT` env vars for tests to read via `env!()`.
- `conformance/<lane>/` — opt-in lanes running real third-party SDKs against
  real mocks. See `conformance/README.md`.
- `skill/wiremirage/` + `skill/wiremirage-debug/` — the product skill shipped
  to *users*. `.claude/skills/wm-dev/` is the skill for developing *this
  repo*; don't confuse them.
- `wit/wiremirage.wit` — the handler contract. `wit/engine.wit` — the
  engine-internal world (source dispatch, response streaming, callbacks).

## Commands

```sh
just check        # fmt check + clippy -D warnings + tests (no Docker tests)
just check-all    # + tier-3 Valkey testcontainers suite
just test         # workspace tests only
just test-valkey  # tier-3 only (Docker)
just audit        # cargo-deny: advisories, licenses, bans, sources (network)
just build
just run-host / run-web / run-web-fast / run-cli <args>
just conformance [lane]
```

Run the host locally:

```sh
WM_STORAGE=memory WM_BOOTSTRAP_TOKEN=wmt_dev_local \
  WM_BOOTSTRAP_EMAIL=admin@local cargo run -p wm-host
```

Mock traffic goes to the group subdomain (`-H 'Host: demo.localhost'`); the
apex is control-plane only.

## Required tooling

Stable Rust (edition 2024, no MSRV pin), plus `rustup target add
wasm32-unknown-unknown`, `cargo install just wasm-tools`, and **Docker** —
`cargo build` invokes the js-engine Dockerfile. `WM_JS_ENGINE_WASM_OVERRIDE`
skips that for no-Docker contexts.

## Testing tiers

The suite is a pyramid and each tier earns its keep:

1. **Unit tests** in-module — matching, patterns, filters, auth rules,
   transpile errors. Fast, the bulk of the coverage.
2. **Tier-2 integration** in `crates/wm-host/tests/` — a real host in-process
   over in-memory storage, driving REST / MCP / UI end to end. This is where
   behaviour contracts live; a new surface feature should land with tests
   here.
3. **Tier-3** (`just test-valkey`) — the same storage suite against real
   Valkey containers. Only for storage semantics.
4. **Conformance** (`just conformance`) — real SDKs, opt-in, not in `check`.

## Where the design lives

- **[docs/adr/](docs/adr/index.md)** — 39 ADRs, in the repo. Read the relevant
  one before changing anything it covers.
- The longer design docs (`route-model.md`, `storage-model.md`,
  `script-api-wit.md`, `rest-api.md`, `mcp-surface.md`, `cli-design.md`,
  `web-ui-design.md`, `auth-and-authz.md`, `user-model.md`) live in the
  maintainer's private Arkiv workspace `wiremirage`, along with the ADR
  originals. Contributors without access should treat the code plus the ADRs
  as the contract; maintainers should keep both in step. Agents with Arkiv
  access: **treat that workspace as read-only** unless the user explicitly
  asks for a write.
- `docs/adr/` is a **snapshot**, refreshed by `just export-adrs` (maintainer
  only). Fix a wrong ADR upstream in Arkiv and re-export; never edit the
  snapshot in place, or the next export silently reverts it.
- `wit/wiremirage.wit` is a verbatim mirror of `script-api-wit.md`. Change the
  design doc first.

## Conventions

- Latest stable Rust, edition 2024. Clippy is `-D warnings` in CI — fix lints
  rather than allowing them.
- Significant decisions get an ADR before implementation. Numbering is
  sequential and never reused; supersessions rewrite the old ADR in place with
  a pointer.
- No `_unused` renames, no "kept for backwards compat" shims, no decorative
  scaffolding. Delete dead code. Pre-1.0 means breaking changes are allowed
  when they make the design better — say so in the commit and update every
  surface in the same change.
- No silent fallbacks in configuration. Missing required config fails fast at
  startup with a message naming what to set.
- Apache-2.0. New files need no license header.

### Two rules that keep the surfaces honest

**Cross-surface parity.** A capability should exist on the CLI, MCP, and UI
unless there's a real interface reason it can't (live tailing is push-only, so
it's MCP+UI; user management is deliberately CLI+UI). "CLI-only" is a gap to
close, not a design.

**The product skill tracks the CLI.** `skill/wiremirage/SKILL.md`, its
`scripts/`, and `skill/wiremirage-debug/SKILL.md` describe the current
surface. Any added, renamed, or removed `wm` subcommand or flag — or any
handler-API change — updates them in the same commit. The same goes for
`docs/`: a change that makes a documented statement false is not finished
until the doc is fixed.

## Slash commands

- `/check` — run the full check suite and report
- `/new-adr` — scaffold an ADR following the established conventions

## Subagents

Prefer `Explore` for codebase searches and `Plan` for non-trivial design work.
There are no repo-tuned subagents.
