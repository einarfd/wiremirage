# WireMirage

**A programmable mock HTTP server for testing, built for AI coding agents to
drive.**

You give WireMirage a TypeScript function; it gives you back a live HTTP
endpoint your system under test can call. Handlers are real code — they hold
state between requests, sleep to simulate latency, stream Server-Sent Events,
and fire webhooks back at your SUT. They run as WebAssembly in a sandbox, so
"real code" doesn't mean "arbitrary code on your box".

```ts
// A rate limiter that starts failing after the third call.
export function handle(req, routeStore, groupStore) {
  const n = routeStore.incr("calls", 1n);
  if (n > 3n) {
    return { status: 429, headers: [["retry-after", "30"]], body: new Uint8Array() };
  }
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify({ ok: true, call: Number(n) })),
  };
}
```

```sh
wm routes add --group stripe-mock --method POST --path /v1/charges \
  --source-file handler.ts
```

That's the whole loop: no compile step, no rebuild, no restart.

## Why this exists

Mocking libraries live inside your test process. That's fine until the thing
you're testing makes real HTTP calls — an SDK, a browser, a container, another
service. WireMirage is for that case, and for one more: an agent writing tests
needs a mock it can *create, inspect, and reason about* without a human in the
loop.

- **Handlers are code, not fixture files.** Stateful flows, conditional
  failures, latency ramps, and streaming responses are ordinary control flow.
- **Every request is journaled.** "Did the SUT call it? With what body?" is a
  query, not a print statement. Requests that matched nothing land in a
  separate log with "did you mean…?" hints.
- **Agent-native surfaces.** A CLI with `--json` on every command, an MCP
  server with 33 tools, and a bundled skill that teaches the workflow.
- **Ephemeral by design.** Groups carry a TTL and cascade-delete everything
  they own. Nothing to clean up, nothing to back up.
- **Multi-tenant.** Each group gets its own subdomain and its own path
  namespace, so two people mocking the same API don't collide.

## Quickstart

Requires Docker and a stable Rust toolchain — see [building](#building).

```sh
docker compose up -d          # Valkey

WM_STORAGE=redis://localhost:6379 \
WM_BOOTSTRAP_TOKEN=wmt_dev_local \
WM_BOOTSTRAP_EMAIL=you@example.com \
  cargo run -p wm-host

# In another shell:
export WM_HOST=http://localhost:8080 WM_TOKEN=wmt_dev_local
cargo install --path crates/wm-cli

wm groups create demo
cat > /tmp/hello.ts <<'EOF'
export function handle(req) {
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify({ hi: req.method })),
  };
}
EOF
wm routes add --group demo --method POST --path /v1/charges --source-file /tmp/hello.ts
```

Mock traffic is served on the **group's subdomain** — `demo.localhost:8080`
here. The apex (`localhost:8080`) is control-plane only. No DNS is needed
locally; the `Host` header alone selects the group:

```sh
curl -X POST -H 'Host: demo.localhost' http://localhost:8080/v1/charges -d '{}'
# {"hi":"POST"}

wm journal list demo          # the request you just made
```

Then open `http://localhost:8080/ui/` for the web UI (`just run-web` boots the
host with dev credentials), or point an agent at the MCP endpoint.

## The surfaces

Everything below talks to the same host and the same authorization rules.

| Surface | For | Start here |
|---|---|---|
| **`wm` CLI** | humans, scripts, CI | [docs/cli.md](docs/cli.md) |
| **MCP server** at `/api/mcp` | AI coding agents | [docs/mcp.md](docs/mcp.md) |
| **Web UI** at `/ui/` | inspection, debugging, editing handlers | browse it |
| **REST API** at `/api/*` | anything else | the CLI wraps it 1:1 |
| **Skill** at [`skill/wiremirage/`](skill/wiremirage/) | teaching an agent the workflow | `SKILL.md` + 5 runnable scripts |

Mock traffic itself is never authenticated — systems under test don't carry
credentials. Everything else is behind a bearer token or a browser session.

## Writing handlers

The short version: export `handle(req, routeStore, groupStore)`, return
`{ status, headers, body }`. Two stores (route-private and group-shared)
persist between requests. `host.sleep`, `host.responseStream`, and
`host.scheduleCallback` cover latency, streaming, and outbound webhooks.

The full guide is **[docs/handlers.md](docs/handlers.md)**; the live contract
is `wm capabilities <topic>` (or the `get_capabilities` MCP tool), which reads
from the running host, and `wit/wiremirage.wit` for the WIT definition.

## How it works

```
        ┌──────────────┐   TypeScript/JS source
        │ wm CLI / MCP │ ─────────────────────────┐
        │  UI / REST   │                          ▼
        └──────────────┘              ┌────────────────────────┐
                                      │  wm-host (axum)        │
  SUT ──── {group}.apex/path ────────▶│  route table + matcher │
                                      │  wasmtime sandbox      │
                                      │  journal / state       │
                                      └───────────┬────────────┘
                                                  │
                                            Valkey (or in-memory)
```

TypeScript is transpiled to JavaScript in-process with swc, then dispatched
through a shared WebAssembly engine component (StarlingMonkey, built at
compile time and embedded in the binary), instantiated fresh per request. Fuel
metering, an epoch deadline, and a memory cap bound every call. Per-route and
per-group state live in Valkey behind TTLs; so do journal entries.

The reasoning behind each of those choices is in
**[docs/adr/](docs/adr/index.md)** — 37 decision records covering the runtime,
the storage model, auth, routing, and the agent surfaces.

## Documentation

- [Handlers](docs/handlers.md) — the handler API, path patterns, state,
  streaming, callbacks, limits
- [CLI](docs/cli.md) — install, profiles, command tour
- [MCP](docs/mcp.md) — client setup and the tool surface
- [Configuration](docs/configuration.md) — every environment variable
- [Deployment](docs/deployment.md) — DNS/TLS, the container image, hardening
- [Observability](docs/observability.md) — traces and metrics, and which
  answers what
- [ADRs](docs/adr/index.md) — why things are the way they are
- [Contributing](CONTRIBUTING.md) · [Security policy](SECURITY.md)

## Building

Latest stable Rust (pinned via `rust-toolchain.toml`), plus:

```sh
rustup target add wasm32-unknown-unknown
cargo install just wasm-tools
```

**Docker is a build dependency**: the host's `build.rs` runs
`compiler/js-engine/Dockerfile` to produce the shared `js-engine.wasm`
component and embeds it in the binary. The image is layer-cached and the step
only re-runs when `compiler/js-engine/` changes. Set
`WM_JS_ENGINE_WASM_OVERRIDE=/abs/path/to/prebuilt.wasm` to skip Docker and use
a pre-built artifact.

```sh
just check        # fmt + clippy + tests
just build
just check-all    # adds the Valkey-backed tier-3 tests (Docker)
just conformance  # real third-party SDKs against real mocks (Docker)
```

The [conformance lanes](conformance/README.md) run actual client libraries —
the `openai` Python SDK, the AWS Go SDK — against WireMirage mocks, because
the SDK is the only honest judge of whether the bytes are right.

## Layout

```
crates/wm-core/     shared types, REST client, auth
crates/wm-host/     the server: axum + wasmtime + Valkey; MCP under src/mcp/
crates/wm-cli/      the wm binary
crates/wm-transpile/ TypeScript -> JavaScript (swc), shared by the runtime
                    handler path and the engine build
compiler/js-engine/ TypeScript shim + Dockerfile producing js-engine.wasm
wit/                the handler contract (wiremirage.wit) and engine world
types/              the handler contract as TypeScript, for handler authors
skill/              the user-facing skill shipped to agents
conformance/        opt-in lanes running real SDKs against real mocks
docs/               documentation and ADRs
```

## Status

Pre-1.0 and honest about it: no released binaries yet, no version tags, and
breaking changes land without a deprecation window when they make the design
better. The maintainer runs it daily against real SDK integrations, and the
test suite (780+ tests, plus the conformance lanes) gates every change.

Known limits worth knowing before you deploy: **one replica**
([ADR-0037](docs/adr/0037-multi-replica-readiness.md) is the plan for lifting
that), TypeScript/JavaScript are the only handler languages, and the browser
login paths assume a TLS edge in front.

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
