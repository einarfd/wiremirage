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

CI installs all three. Locally, install once.

## Standard loop

1. **Read the spec before changing behavior.** Start at `index.md`, drill
   into the doc the change touches (`route-model.md`, `storage-model.md`,
   `script-api-wit.md`, `cli-design.md`, etc.) and the relevant ADR.
2. **Make the code change.** Prefer editing existing files over adding new
   ones. Follow the rest of the repo's style (rustfmt, clippy `-D warnings`).
3. **Run `just check`.** Fix what's broken before reporting.
4. **If the code conflicts with the spec, surface it.** Either propose a
   spec update (via `/new-adr` if it's a real decision) or revise the code.
   Don't silently diverge.

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
