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
`compiler/typescript/`). Routes are mutable via `PATCH
/__api/routes/{group}/{n}` (slice 15): `methods`, `path`, and the
handler artifact swap together; path/method changes re-run pattern
conflict detection, and any wasm swap evicts the in-memory component
cache. Per-route state can be inspected and cleared via `GET/DELETE
/__api/routes/{group}/{n}/state`, and a route's handler can be run
against a synthetic request via `POST .../{n}/dry-run` — dry-run
snapshots route + group kv into a `dryrun:{run_id}:` namespace so
writes are isolated and discarded on completion, no journal entry
created (slice 16). Bearer-token auth gates the `/__api/*` surface;
mock traffic to user routes stays open by design (SUTs don't have
tokens).
Bootstrap with `WM_BOOTSTRAP_TOKEN=wmt_...` on first startup. Token
and user management live at `/__api/tokens` and `/__api/users`
(admin-only for cross-user actions; `GET /__api/users/me` for self).
For browser login on testing or private deployments, set
`WM_LOCAL_AUTH=alice:hunter2:admin,bob:pw` + `SESSION_SECRET`; the
`/__auth/login/password` endpoint then mints an `wm_session` cookie
that authenticates `/__api/*` alongside the bearer-token path
(slice 20, per ADR-0018 — not for public exposure). The web UI
landed in slice 21 — run `just run-web` and open
`http://localhost:8080/__ui/` in a browser. Today the home page +
login + navigation are real; detail pages land in slices 22–26.
Every dispatched mock request and every unmatched request is journaled
in Valkey (default 1h TTL); fetch via `GET /__api/journal/{group}` and
`GET /__api/unmatched` (admin-only). Groups are first-class lifecycle
units with TTL (default 24h, sliding-on-traffic by default); explicit
DELETE cascades routes, kv/gkv state, and journal entries together,
and a background sweeper reaps groups that hit their TTL. The `wm` CLI
wraps the REST surface end-to-end: groups, routes (including `wm
routes update`, `wm routes state`, `wm routes test`), journal,
tokens, and the public probes — see "Using the CLI" below. The MCP
server is part of the host and mounts at `/__api/mcp` over the
streamable-HTTP transport; 20 tools cover identity, discovery,
group/route CRUD (now including `update_route`), per-route state
(`show_route_state`, `clear_route_state`), dry-run (`dry_run_route`),
the live-tail streaming pair (`wait_for_request`, `tail_journal`),
and the match probe (`find_route`, mirrored by `wm match` and `GET
/__api/match`). All behind the same bearer-token auth. Live tail
also exposes `GET /__api/journal/tail` as an SSE endpoint for non-MCP
consumers.

## Layout

```
crates/
  wm-core/                shared types, REST client, auth
  wm-host/                long-running Rust server (axum + wasmtime + Valkey)
                          MCP service lives under wm-host/src/mcp/
  wm-cli/                 the wm CLI binary
compiler/
  typescript/             Node-based compiler sidecar (componentize-js + jco)
wit/
  wiremirage.wit          handler script API contract (mirrors the design doc)
skill/
  wiremirage/             user-facing Anthropic Skill (SKILL.md + scripts)
  wiremirage-debug/       diagnostic sub-skill triggered on mock-debugging tasks
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

## Using the CLI

The `wm` binary wraps the REST API. Auth and host URL come from the
environment by default:

```
export WM_HOST=http://localhost:8080      # default; override for a remote host
export WM_TOKEN=wmt_dev_local             # bearer token (matches the host's bootstrap)
```

Both can also be passed inline as `--host` / `--token`. Health and
version probes work without a token; everything else requires one.

```
wm health                                  # probes /__health
wm groups create stripe-mock               # default 24h sliding TTL
wm routes add --group stripe-mock --method POST --path /v1/charges \
  --source-file handler.ts                 # compiles via the sidecar
wm routes list
wm routes update stripe-mock/1 --source-file new-handler.ts  # PATCH
wm routes source stripe-mock/1             # print stored handler source
wm routes state stripe-mock/1              # list per-route kv
wm routes test stripe-mock/1 --method POST # dry-run (no journal, isolated state)
wm journal list stripe-mock                # newest first, paginated
wm tokens create ci-runner                 # plaintext printed once
wm groups delete stripe-mock --force       # cascades routes, kv, journal
```

Pass `--json` on any command for machine-parseable output (the contract
for scripts and agents); the default human format is column-aligned text.
Exit codes: `0` ok, `1` generic error, `2` clap usage error, `4` auth, `5`
not-found, `6` conflict.

`wm completion bash|zsh|fish|powershell` emits a completion script for
your shell — pipe it into the appropriate location (e.g.
`/etc/bash_completion.d/wm`, `${fpath[1]}/_wm`).

Admins manage users via `wm users list/show/me/create/update/delete`.
By design user management is CLI-only; the MCP server does not expose
it (per `mcp-surface.md`).

What this slice ships and what it doesn't is captured in
`cli-design.md` (private design doc). Notable deferrals: profiles /
dotenv / `--config-file`, color, shell completions, `--from-file` body
input, `wm journal tail`, `wm match`, route `update` / `test` / `state`,
and admin user CRUD. Everything else from the spec is wired up.

## Using the MCP server

The host exposes an MCP (Model Context Protocol) service at
`/__api/mcp` using the streamable-HTTP transport. Authentication is
the same bearer token used for the REST API and the CLI.

Add the server to Claude Code:

```sh
claude mcp add --transport http wiremirage \
  https://wm.example.com/__api/mcp \
  --header "Authorization: Bearer wmt_..."
```

The current surface is 21 tools — identity (`who_am_i`), discovery
(`summarize_workspace`, `list_recent_unmatched`, `find_route`),
group CRUD (`list_groups`, `show_group`, `create_group`,
`delete_group`, `refresh_group_ttl`), route CRUD (`list_routes`,
`show_route`, `show_route_source`, `create_route`, `update_route`,
`delete_route`), state + dry-run (`clear_group_state`,
`show_route_state`, `clear_route_state`, `dry_run_route`), and the
slice-11 streaming pair (`wait_for_request`, `tail_journal`). The streaming tools
subscribe to a single-host broadcast bus inside the host and return
accumulated entries when their stop condition fires (count + timeout
for `wait_for_request`; max_entries + idle timeout for
`tail_journal`). `find_route` mirrors the `wm match` CLI and `GET
/__api/match` REST endpoint shipped in slice 13. `update_route`
(slice 15) is wasm-only at the MCP layer, matching `create_route`;
source-based updates go through REST or `wm routes update
--source-file`. `dry_run_route` (slice 16) snapshots route + group
state into a per-run namespace so the handler can read/write without
mutating the real store, and discards the snapshot on completion.
Multi-host pub/sub for the bus lands in a follow-up slice.

## Production hardening

The defaults are tuned for plain-HTTP dev workflows. Before exposing the host
even on a trusted network behind a TLS edge (Caddy, an ALB, nginx with TLS, …),
flip these two flags so the cookie + throttle behavior matches the deployment
shape:

- `WM_SECURE_COOKIES=1` — appends `Secure` to the `wm_session` and `wm_csrf`
  cookies. Browsers will then refuse to send them on plain HTTP, which is what
  you want when every legitimate request reaches you over HTTPS. Leave unset
  for `just run-web-fast` / local-HTTP development.
- `WM_TRUST_FORWARDED_HEADERS=1` — honors `X-Forwarded-For` for the
  per-IP login throttle. Default is off because the header is set by any
  caller and trusting it from a directly-reachable host lets an attacker
  spoof the throttle bucket. Only enable this when a reverse proxy you control
  is the **only** thing that can reach the host (e.g. the host binds to
  `127.0.0.1` and Caddy proxies via `localhost:<port>`).

In addition, the first-deploy checklist:

- **Generate a strong `WM_BOOTSTRAP_TOKEN`** (`openssl rand -hex 32` is fine)
  and treat it as a credential. After first deploy, log in via the bootstrap
  token, mint an operator token (`wm tokens create operator/default`), and
  delete the bootstrap user (`wm users delete bootstrap`) so the literal
  bootstrap token stops being a valid credential.
- **Generate a strong `SESSION_SECRET`** of at least 32 bytes (`openssl rand
  -base64 48`). Rotating it later invalidates every existing session by
  design — so keep it stable unless you intend a global logout.
- **At the TLS edge**: turn on HSTS (`Strict-Transport-Security`), set
  `X-Content-Type-Options: nosniff`, and consider a strict CSP — the UI only
  loads same-origin scripts (Ace is vendored under `/__ui/static/ace/`).
- **Bind the host to `127.0.0.1`** in the deployment compose / systemd unit so
  the only ingress is through the reverse proxy. Combined with
  `WM_TRUST_FORWARDED_HEADERS=1`, the throttle keys to the proxy-reported
  client IP correctly and is not spoofable.

## License

Copyright 2026 Einar Fløystad Dørum. Licensed under the Apache License,
Version 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
