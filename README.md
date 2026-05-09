# WireMirage

An agent-native, multi-language mock server. Handlers are short scripts
(TypeScript first, Python next) that run as WebAssembly components in a
sandboxed runtime. Each route has access to an isolated key-value store;
related routes share state through groups. Routes are ephemeral by default,
reaped via group TTL.

WireMirage is driven primarily through the `wm` CLI and a bundled skill that
teaches workflow patterns to AI coding agents. An MCP server is available for
streaming the live journal and for remote access where installing the CLI
isn't practical. A small web UI exists for human inspection.

## Status

Early implementation. The WIT script API (`wit/wiremirage.wit`) is in
place, the host runs components against it, storage is abstracted behind
in-memory and Valkey backends, routes are stored in a registry keyed by
`{group}/{n}`, and the REST API accepts both pre-compiled wasm uploads
and TypeScript source (the latter goes through a Node sidecar at
`compiler/typescript/`). Bearer-token auth gates the `/__api/*` surface;
mock traffic to user routes stays open by design (SUTs don't have
tokens). Bootstrap with `WM_BOOTSTRAP_TOKEN=wmt_...` on first startup.
Token and user management live at `/__api/tokens` and `/__api/users`
(admin-only for cross-user actions; `GET /__api/users/me` for self).
Every dispatched mock request and every unmatched request is journaled
in Valkey (default 1h TTL); fetch via `GET /__api/journal/{group}` and
`GET /__api/unmatched` (admin-only). Groups are first-class lifecycle
units with TTL (default 24h, sliding-on-traffic by default); explicit
DELETE cascades routes, kv/gkv state, and journal entries together,
and a background sweeper reaps groups that hit their TTL.

## Layout

```
crates/
  wm-core/                shared types, REST client, auth
  wm-host/                long-running Rust server (axum + wasmtime + Valkey)
  wm-cli/                 the wm CLI binary
  wm-mcp/                 the MCP server
compiler/
  typescript/             Node-based compiler sidecar (componentize-js + jco)
wit/
  wiremirage.wit          handler script API contract (mirrors the design doc)
docker-compose.yml        Valkey + sidecar for local development
```

## Building

Requires the latest stable Rust toolchain (pinned via `rust-toolchain.toml`)
and a few extra tools used to build the host's fixture guests:

```
rustup target add wasm32-unknown-unknown
cargo install just wasm-tools
```

Then:

```
just check    # fmt, clippy, test
just build    # cargo build --workspace
```

To run the host with the TypeScript sidecar:

```
docker compose up -d   # starts Valkey + compiler-typescript
WM_BOOTSTRAP_TOKEN=wmt_dev_local \
  WM_STORAGE=redis://localhost:6379 \
  WM_COMPILER_URL=http://localhost:9100 \
  cargo run -p wm-host
# In another shell:
curl -X POST localhost:8080/__api/routes \
  -H 'authorization: Bearer wmt_dev_local' \
  -H content-type:application/json \
  -d '{"methods":["POST"],"path":"/v1/charges","language":"typescript",
       "source":"export function handle(req,_r,_g){return {status:200,headers:[],body:new TextEncoder().encode(\"hi from \"+req.method)};}"}'
# Mock traffic does not need an Authorization header.
curl -X POST localhost:8080/v1/charges -d '{}'
```

The host exposes two unauthenticated probe endpoints for orchestrators:
`GET /__health` (liveness, always 200) and `GET /__ready` (readiness;
checks the configured backends).

Required env vars (no silent fallbacks):

- `WM_STORAGE` — `memory`, `redis://host:port[/db]`, or `rediss://...` for TLS.

On first startup, set `WM_BOOTSTRAP_TOKEN=wmt_...` to provision an admin
user named `bootstrap` whose API token is the supplied plaintext. The
variable is idempotent — set it once, rotate later via `/__api/tokens`.
The host refuses to start if no users exist and no bootstrap token is
supplied.

Optional:

- `WM_COMPILER_URL` — sidecar endpoint. Without it, source-based
  requests fail; pre-compiled `language: "wasm"` uploads still work.
- `OTEL_EXPORTER_OTLP_ENDPOINT` — URL of an OTLP/gRPC collector. When
  set, the host exports spans for the request → handler → backend
  path; when unset, host logging is stderr-only. The standard
  `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` env vars are
  honored. W3C `traceparent` is extracted from incoming requests and
  injected on outbound calls to the sidecar.

Tier-3 tests require Docker:

```
just test-valkey       # Valkey-backed storage suite
just test-sidecar      # builds the sidecar image, runs end-to-end TS test
just check-all         # everything (fmt + clippy + test + tier-3)
```

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
