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

Pre-implementation. The design is captured in a private specification set;
this repository will be populated as the contracts stabilize.

## Layout

```
crates/
  wm-core/   shared types, REST client, auth
  wm-host/   the long-running Rust server (axum + wasmtime + Valkey)
  wm-cli/    the wm CLI binary
  wm-mcp/    the MCP server
```

## Building

Requires the latest stable Rust toolchain (pinned via `rust-toolchain.toml`)
and [`just`](https://github.com/casey/just).

```
just check    # fmt, clippy, test
just build    # cargo build --workspace
```

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
