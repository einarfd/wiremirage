# CLAUDE.md

Bootstrap notes for Claude Code working in this repository.

## What this is

WireMirage: an agent-native, multi-language mock server. Handlers are real
code (TypeScript first), compiled to Wasm components, executed inside a Rust
host (`wasmtime`). Per-route isolated KV state; groups as TTL-bounded
lifecycle units. Storage in Valkey (Redis wire protocol). See `README.md`.

**Status:** slices 1–2 landed. The WIT contract is live at
`wit/wiremirage.wit`, the host (`wm-host`) instantiates components against
it, and storage is abstracted behind a `Storage` enum with both in-memory
and Valkey backends. Routing is still a single hardcoded-component
catch-all. Next slice: a real route table + REST API for creating routes.

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

Cargo workspace with four crates under `crates/`:

- `wm-core` — shared types, REST client, auth
- `wm-host` — long-running Rust server (axum + wasmtime + Valkey)
- `wm-cli` — `wm` CLI binary
- `wm-mcp` — MCP server

The WIT contract that handlers program against lives at `wit/wiremirage.wit`.
It is the verbatim mirror of `script-api-wit.md` in the Arkiv workspace; if
you need to change it, update the design doc first.

Wasm guest fixtures used by the host's tier-2 integration tests live at
`crates/wm-host/tests/fixtures/<name>/` as **standalone crates** (their own
`Cargo.toml` + `Cargo.lock`, excluded from the parent workspace). The host's
`build.rs` compiles them to `wasm32-unknown-unknown` and runs `wasm-tools
component new` on the result; the resulting paths are stamped into env vars
of the form `WM_FIXTURE_<name>_COMPONENT` for tests to read via `env!()`.

The product skill (shipped to *users* of WireMirage) will live at
`skill/wiremirage/` per ADR-0015. The dev skill at `.claude/skills/wm-dev/`
is for *developing this repo* — not the same thing.

## Common commands

Use `just` (see `justfile`):

- `just check` — fmt check + clippy `-D warnings` + tests (skips Docker tests)
- `just check-all` — like `check` plus tier-3 Valkey tests via testcontainers
- `just fmt` — format
- `just test` — workspace tests only (no Valkey)
- `just test-valkey` — tier-3 testcontainers suite, requires Docker
- `just build` — `cargo build --workspace`
- `just run-host` / `just run-cli <args>`

To run the host directly against a fixture component:

```sh
WM_STORAGE=memory \
WM_FIXTURE_WASM=$(find target -name 'echo_handler.component.wasm') \
  cargo run -p wm-host
```

`WM_STORAGE` is required (no silent fallback). Accepts `memory`,
`redis://host:port[/db]`, or `rediss://...` for TLS.

## Required tooling

In addition to a stable Rust toolchain:

- `wasm32-unknown-unknown` target — `rustup target add wasm32-unknown-unknown`
- `wasm-tools` CLI — `cargo install wasm-tools` (used by `wm-host/build.rs`
  to componentize fixture guests; also handy for `wasm-tools component wit
  <component.wasm>` when investigating component-shape issues)
- `just` — `cargo install just`

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
