# Changelog

Notable changes to WireMirage. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

WireMirage is pre-1.0: breaking changes are allowed on minor versions when
they make the design better, and are called out here.

## [Unreleased]

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
