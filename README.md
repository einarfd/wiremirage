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
`{group}/{n}`, and the REST API accepts pre-compiled wasm uploads as
well as `language: "javascript" | "typescript"` source — JS and TS
both dispatch through an embedded shared js-engine.wasm component
(ADR-0020), with TypeScript transpiled to JS in-host via pure-Rust
swc. No external compiler. Routes are mutable via `PATCH
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
  js-engine/              TypeScript shim + Dockerfile that builds the
                          shared js-engine.wasm (ADR-0020) at cargo build
                          time; output lives in target/, not vendored
wit/
  wiremirage.wit          handler script API contract (mirrors the design doc)
  engine.wit              shared-engine world (host imports for source dispatch)
skill/
  wiremirage/             user-facing Anthropic Skill (SKILL.md + scripts)
  wiremirage-debug/       diagnostic sub-skill triggered on mock-debugging tasks
docker-compose.yml        Valkey for local development
```

## Building

Requires the latest stable Rust toolchain (pinned via `rust-toolchain.toml`)
plus:

```
rustup target add wasm32-unknown-unknown
cargo install just wasm-tools
```

Docker is also required — the host's `build.rs` invokes a pinned
`compiler/js-engine/Dockerfile` to produce the shared js-engine.wasm
component (ADR-0020) and embeds it into the binary. The image build is
layer-cached, and cargo only re-runs the step when something under
`compiler/js-engine/` changes.

If you can't run Docker (e.g., building from source on a restricted
machine), set `WM_JS_ENGINE_WASM_OVERRIDE=/abs/path/to/prebuilt.wasm` to
skip the docker invocation and use a pre-built artifact instead.

Then:

```
just check    # fmt, clippy, test
just build    # cargo build --workspace
```

To run the host:

```
docker compose up -d   # starts Valkey
WM_BOOTSTRAP_TOKEN=wmt_dev_local \
  WM_STORAGE=redis://localhost:6379 \
  cargo run -p wm-host
# In another shell:
curl -X POST localhost:8080/__api/routes \
  -H 'authorization: Bearer wmt_dev_local' \
  -H content-type:application/json \
  -d '{"methods":["POST"],"path":"/v1/charges","language":"typescript",
       "source":"function handle(req,_r,_g){return {status:200,headers:[],body:new TextEncoder().encode(\"hi from \"+req.method)};}"}'
# Mock traffic does not need an Authorization header.
curl -X POST localhost:8080/v1/charges -d '{}'
```

TypeScript and JavaScript source compile in-host — TS is transpiled via
swc and dispatched through an embedded `js-engine.wasm` component (see
ADR-0020). No Node sidecar.

The host exposes two unauthenticated probe endpoints for orchestrators:
`GET /__health` (liveness, always 200) and `GET /__ready` (readiness;
checks the configured backends).

Tier-3 tests require Docker:

```
just test-valkey       # Valkey-backed storage suite
just check-all         # everything (fmt + clippy + test + tier-3)
```

## Configuration

All configuration is via environment variables — no config file. The host
fails fast on missing required values rather than silently falling back, so
a misconfigured deploy surfaces at startup, not on the first failed request.

Authentication splits into **three independent paths** that can be enabled
in any combination:

| Path | Used by | Required env vars |
|---|---|---|
| **API tokens (bearer)** | `wm` CLI, MCP server, scripts, agents | `WM_BOOTSTRAP_TOKEN` (first start) |
| **GitHub OAuth** | Browser users | `WM_GITHUB_CLIENT_ID`, `WM_GITHUB_CLIENT_SECRET`, `WM_GITHUB_ALLOW_USERS` and/or `WM_GITHUB_ALLOW_ORGS`, `SESSION_SECRET` |
| **Local password** | Testing / trusted-network only (ADR-0018) | `WM_LOCAL_AUTH`, `SESSION_SECRET` |

API tokens always work. GitHub OAuth and local password are independent —
enable either, both, or neither. Mock traffic (everything not under a
reserved `/__api/`, `/__ui/`, `/__auth/` prefix) is always unauthenticated by
design — SUTs don't have credentials.

### Storage (required)

- `WM_STORAGE` — one of:
  - `memory` — in-process, state lost on restart. Fine for `just run-web-fast`
    and integration tests.
  - `redis://host:port[/db]` — Valkey / Redis. The recommended deployment shape.
  - `rediss://host:port[/db]` — same, with TLS.

### Listener (optional)

- `WM_LISTEN_ADDR` — default `127.0.0.1:8080`. The release Docker image overrides
  this to `0.0.0.0:8080` so the container is reachable when published with
  `-p 8080:8080`. Production deployments behind a reverse proxy should bind to
  `127.0.0.1` (combined with `WM_TRUST_FORWARDED_HEADERS=1` — see *Production
  hardening* below).

### API tokens — bootstrap (required on first start)

`WM_BOOTSTRAP_TOKEN=wmt_<some-secret>` creates an admin user named `bootstrap`
on the very first host startup, with the supplied plaintext as their API
token. Subsequent starts with the same env var are no-ops *as long as the
bootstrap user still exists*. The host **refuses to start** if no users
exist and the variable is unset — this prevents a fresh deployment from
coming up unreachable.

Generate one with `openssl rand -hex 32` (prefix with `wmt_` to match the
project's token convention). After first deploy, log in with this token
via the CLI (`WM_TOKEN=wmt_... wm health`) or the UI's bearer-token flow,
mint a real operator token (`wm tokens create operator/default`), then
delete the bootstrap user. Note that an admin can't self-delete (built-in
guard), so the deletion happens from a *different* admin's session — either
a second user you created, or your OAuth-provisioned user once GitHub
login has succeeded once.

**Important — drop `WM_BOOTSTRAP_TOKEN` after retirement.** The host's
bootstrap check only verifies "does a user named `bootstrap` exist?" — if
the env var stays set after you delete the bootstrap user, the *next*
host restart silently re-creates it with the same token. Always unset the
env var when retiring the bootstrap user; treat the two as a paired
operation.

### Browser login — GitHub OAuth (recommended for production)

Two steps. First, register a GitHub **OAuth App** (the simple flavor —
not a GitHub App, see below).

> **OAuth App vs GitHub App — pick OAuth App.**  GitHub's developer
> settings has two different things you can create:
>
> - **OAuth Apps** (Settings → Developer settings → **OAuth Apps**) — the
>   classic "sign in with GitHub" flow. Only three things to configure:
>   name, homepage URL, callback URL. This is what WireMirage uses.
> - **GitHub Apps** (Settings → Developer settings → **GitHub Apps**) — a
>   much richer system designed for apps that act on repos (CI, bots,
>   integrations). Has Webhooks, Permissions, Event subscriptions, an
>   installation flow, "Where can this be installed", post-install
>   callbacks, and so on. **Don't use this** — none of it applies to
>   "let me read this user's GitHub identity," and the extra surface is
>   pure overhead.
>
> If you're staring at a form that asks about webhooks or permissions,
> you're on the wrong page — go back one step to the developer-settings
> landing and pick **OAuth Apps** instead.

Steps:

1. **Settings → Developer settings → OAuth Apps → New OAuth App** (under
   your personal account at `https://github.com/settings/developers`, or
   under an org if the WireMirage instance is for a team —
   `https://github.com/organizations/<org>/settings/applications/new`).
2. Fill in:
   - **Application name**: anything you'll recognize (e.g.
     `wiremirage-staging`). Shown on the GitHub OAuth consent screen.
   - **Homepage URL**: `https://wm.example.com` — whatever public URL
     your WireMirage will live at. Shown on the consent screen as a
     "more info" link.
   - **Application description** (optional): a sentence visible on the
     consent screen. Empty is fine.
   - **Authorization callback URL**: `https://wm.example.com/__auth/callback`
     — exact match, including the path. The host computes this URL itself
     from the inbound request's `X-Forwarded-*` headers (Caddy or
     whichever reverse proxy you run populates them), so it must agree
     with what GitHub sees. If TLS terminates somewhere other than
     `https://wm.example.com`, you'll see a `redirect_uri mismatch` error
     from GitHub; fix the callback or the proxy headers until they
     agree.
   - **Enable Device Flow**: leave **unchecked**. Device flow is for
     headless logins (CLI tools without a browser). WireMirage's login
     is a browser round-trip; device flow isn't used.
3. **Generate a client secret** on the app's settings page. Copy both
   the Client ID (visible immediately) and the secret (shown once —
   stash it before navigating away).

Then set these env vars on the host:

- `WM_GITHUB_CLIENT_ID` — the OAuth app's Client ID.
- `WM_GITHUB_CLIENT_SECRET` — the generated secret. Treat as a credential.
- `WM_GITHUB_ALLOW_USERS` — comma-separated GitHub logins allowed to log in
  (e.g. `alice,bob`). OR'd with `WM_GITHUB_ALLOW_ORGS`.
- `WM_GITHUB_ALLOW_ORGS` — comma-separated GitHub org logins; any member of
  any listed org is allowed in. Requires the `read:org` scope, which the host
  always requests. Use this for "anyone on the team" semantics.
- `WM_GITHUB_ADMIN_USERS` — optional subset of allowed users (by GitHub
  login) promoted to admin on first login. When empty, every GitHub user
  lands as a non-admin and existing admins can promote via
  `wm users update <login> --admin`.
- `SESSION_SECRET` — HMAC key for `wm_session` and `wm_csrf` cookies. At
  least 32 bytes; `openssl rand -base64 48` works. Rotating invalidates
  every existing session, so keep it stable unless you mean to log everyone
  out.

If `WM_GITHUB_CLIENT_ID` is set but `WM_GITHUB_CLIENT_SECRET` is missing (or
vice versa), the host refuses to start — a half-configured OAuth path is a
silent footgun otherwise. If neither is set, the GitHub flow simply isn't
enabled and the login page omits the "Continue with GitHub" button.

Denied logins (allow-list miss) get a clear error after the OAuth round-trip
rather than landing in an authenticated session — so a leaked Client ID
doesn't grant access on its own.

### Browser login — local passwords (testing / trusted networks only)

- `WM_LOCAL_AUTH=alice:hunter2:admin,bob:correct-horse-battery-staple` —
  comma-separated `user:password[:role]` triples; `role` is `admin` or omitted
  (default user). Passwords are argon2id-hashed at startup; the plaintext
  is never persisted.
- `SESSION_SECRET` as above.

This mode exists for testing and trusted-network deployments — passwords in
env vars aren't OAuth-grade. Don't expose a host with `WM_LOCAL_AUTH` set to
the public internet without a TLS edge **and** an IP allow-list at the
reverse proxy.

### Observability (optional)

- `OTEL_EXPORTER_OTLP_ENDPOINT` — URL of an OTLP/gRPC collector (e.g.
  `http://localhost:4317`). When unset, the host logs to stderr only; when
  set, spans are exported in addition. No localhost fallback if the env var
  is missing — the absence of an endpoint is taken as "don't try."
- `OTEL_SERVICE_NAME` — default `wm-host`. Override to disambiguate multiple
  WireMirage instances in the same backend.
- `OTEL_RESOURCE_ATTRIBUTES` — standard OTel SDK behavior; comma-separated
  `key=value` pairs (e.g. `deployment.environment=prod,service.version=abc123`).

Inbound W3C `traceparent` is extracted on every request and used as the
dispatch span's parent so the host's spans chain under whatever upstream
traced the request.

## Using the CLI

### Installing

No pre-built binaries today — `cargo install` from source is the install
path. The CLI is a thin Rust binary with no native dependencies (it
shells out to the host's REST API), so it's quick to compile compared
to the host itself.

From a local clone:

```
cargo install --path crates/wm-cli
```

Without a clone, directly from the repo (needs git access since the
repo is currently private):

```
cargo install --git https://github.com/einarfd/wiremirage.git --branch main wm-cli
```

That puts `wm` in `~/.cargo/bin/` — make sure that's on your PATH.
`wm --version` confirms.

Shell completions: `wm completion bash|zsh|fish|powershell` emits a
completion script — pipe it into the right place for your shell
(e.g. `/etc/bash_completion.d/wm`, `${fpath[1]}/_wm`).

Pre-built binaries via GitHub releases land when v0.1.0 ships.

### Using

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
  --source-file handler.ts                 # transpiled in-host
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
