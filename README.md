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
`compiler/typescript/`). No real auth yet — the host requires
`WM_INSECURE_NO_AUTH=1` to acknowledge that.

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
WM_INSECURE_NO_AUTH=1 \
  WM_STORAGE=redis://localhost:6379 \
  WM_COMPILER_URL=http://localhost:9100 \
  cargo run -p wm-host
# In another shell:
curl -X POST localhost:8080/__api/routes -H content-type:application/json \
  -d '{"methods":["POST"],"path":"/v1/charges","language":"typescript",
       "source":"export function handle(req,_r,_g){return {status:200,headers:[],body:new TextEncoder().encode(\"hi from \"+req.method)};}"}'
curl -X POST localhost:8080/v1/charges -d '{}'
```

Required env vars (no silent fallbacks):

- `WM_STORAGE` — `memory`, `redis://host:port[/db]`, or `rediss://...` for TLS.
- `WM_INSECURE_NO_AUTH=1` — acknowledges that the REST API is open
  without authentication.

Optional:

- `WM_COMPILER_URL` — sidecar endpoint. Without it, source-based
  requests fail; pre-compiled `language: "wasm"` uploads still work.

Tier-3 tests require Docker:

```
just test-valkey       # Valkey-backed storage suite
just test-sidecar      # builds the sidecar image, runs end-to-end TS test
just check-all         # everything (fmt + clippy + test + tier-3)
```

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
