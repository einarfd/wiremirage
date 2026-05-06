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
in-memory and Valkey backends, and routes are stored in a registry keyed
by `{group}/{n}`. The slice-3 REST API at `/__api/routes` accepts
pre-compiled wasm components today; the source-based path waits on the
compiler sidecar slice. No real auth yet — the host requires
`WM_INSECURE_NO_AUTH=1` to acknowledge that.

## Layout

```
crates/
  wm-core/   shared types, REST client, auth
  wm-host/   the long-running Rust server (axum + wasmtime + Valkey)
  wm-cli/    the wm CLI binary
  wm-mcp/    the MCP server
wit/
  wiremirage.wit    handler script API contract (mirrors the design doc)
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

To run the host and register a route:

```
WM_INSECURE_NO_AUTH=1 WM_STORAGE=memory cargo run -p wm-host
# In another shell:
WASM=$(base64 -w0 < $(find target -name echo_handler.component.wasm | head -1))
curl -X POST localhost:8080/__api/routes -H content-type:application/json \
  -d "{\"methods\":[\"POST\"],\"path\":\"/v1/charges\",\"language\":\"wasm\",\"bindings_version\":\"0.1.0\",\"compiled_wasm\":\"$WASM\"}"
curl -X POST localhost:8080/v1/charges -d '{}'
```

Required env vars (no silent fallbacks):

- `WM_STORAGE` — `memory`, `redis://host:port[/db]`, or `rediss://...` for TLS.
- `WM_INSECURE_NO_AUTH=1` — acknowledges that the REST API is open
  without authentication. Real auth lands in a follow-up slice.

Tier-3 tests (against a real Valkey container via testcontainers-rs)
require Docker:

```
just test-valkey      # or: cargo test -p wm-host --features valkey-tests
```

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
