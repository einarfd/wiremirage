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

Early implementation. The WIT script API (`wit/wiremirage.wit`) is in place
and the host can instantiate components against it; storage is in-memory
and routing is a single hardcoded handler. Valkey-backed storage, real
route tables, and the CLI come next.

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

To run the host directly against the bundled echo fixture:

```
WM_FIXTURE_WASM=$(find target -name 'echo_handler.component.wasm') \
  cargo run -p wm-host
```

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
