---
name: wiremirage
description: Use this skill when you need to mock HTTP services for testing. WireMirage runs a programmable mock server reachable from the system under test; mocks are TypeScript handlers running as WebAssembly with persistent state between requests. Reach for this when unit-level mocking won't work because the SUT needs to make real HTTP calls — e.g., end-to-end tests, integration tests against third-party APIs (Stripe, GitHub, internal services), or simulating flaky/error conditions over the network. Don't use it for in-process mocking that fits inside your test framework.
---

# WireMirage skill

WireMirage is a programmable mock HTTP server. You write small TypeScript handlers that the host compiles to WebAssembly and runs in a sandbox. Each route has its own key-value store; related routes share a "group" with a TTL. This skill teaches the workflow patterns; the actual operations happen via the `wm` CLI through your Bash tool.

## When to reach for this skill

- The system under test makes outbound HTTP calls and you can't (or don't want to) intercept them inside the test process.
- You're testing error handling, retries, rate limits, or other behaviors that need a *real* HTTP endpoint behaving in controlled ways.
- The same mock setup needs to live across multiple test runs or processes (e.g. CI, multiple developer machines, multi-language test stacks).

## When NOT to reach for this skill

- You're writing a unit test where in-process mocks (`jest.mock`, `unittest.mock`, or similar) would do.
- You need full traffic recording/replay — that's a separate workflow on top of WireMirage and not built in.
- The SUT can be redirected to call your test code directly — simpler is better.

## Model in one paragraph

A **route** matches on `(method, path-pattern)` and runs a TypeScript handler that returns a response. Routes live inside a **group** (TTL-bounded — default 24h, sliding by default; explicit DELETE cascades all the group's state). The handler has access to two stores: a per-route key-value store and a per-group shared store. Groups + routes are addressed by ULIDs internally and by `{group}/{n}` slugs externally (e.g. `stripe-mock/7`). Mock traffic doesn't need a token; the admin API at `/__api/*` does.

## Setup

```sh
export WM_HOST=http://localhost:8080      # default; override for a remote host
export WM_TOKEN=wmt_...                   # bearer token from your WireMirage admin
wm health                                 # confirms the host is reachable
```

`WM_TOKEN` and `WM_HOST` can also be passed inline as `--token` / `--host`. `wm health` and `wm version` work without a token; everything under `wm groups`, `wm routes`, `wm journal`, `wm tokens` requires one.

`--json` is available on every command — switch to it when scripting or when you want to feed output into `jq`. The default human format is for reading.

## The basic workflow

```sh
# Create a group with a 1-hour TTL.
wm groups create stripe-mock --ttl-seconds 3600

# Write a handler in a file.
cat > /tmp/charge.ts <<'EOF'
export function handle(req, routeStore, groupStore) {
  const id = "ch_" + Math.random().toString(36).slice(2);
  const body = JSON.stringify({ id, amount: 1000, status: "succeeded" });
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(body),
  };
}
EOF

# Add a route pointing at the handler. The host transpiles TypeScript
# to JavaScript in-process (via swc) and dispatches both languages
# through the embedded js-engine.wasm (ADR-0020). No external compile
# step, no Node sidecar.
wm routes add --group stripe-mock --method POST --path /v1/charges \
  --source-file /tmp/charge.ts

# Tweak it in place (e.g., swap the handler source). Same flags as
# `add`; pass only what you want to change. Owner-or-admin only.
# wm routes update stripe-mock/1 --source-file /tmp/charge-v2.ts

# Print the handler source for a route. Prints the literal source
# to stdout. Owner-or-admin only.
# wm routes source stripe-mock/1

# Inspect / clear the route's private kv state (useful between test
# phases when you want a clean slate without re-creating the route).
# wm routes state stripe-mock/1                 # list
# wm routes state stripe-mock/1 --clear         # wipe

# Dry-run the handler against a synthetic request: see what it
# returns without involving the SUT, with state reads/writes
# happening against a discarded snapshot. No journal entry.
# wm routes test stripe-mock/1 --method POST --body '{"x":1}'

# Run your test that hits http://$WM_HOST/v1/charges. Mock traffic is
# unauthenticated.
curl -X POST $WM_HOST/v1/charges -d '{}'

# Check the journal to confirm the SUT actually called the mock.
wm journal list stripe-mock

# Tear down.
wm groups delete stripe-mock --force
```

## Common patterns

The `scripts/` directory next to this `SKILL.md` ships ready-to-run examples you can invoke directly via Bash, adapt by editing, or extract patterns from:

- **`scripts/setup-stripe-mock.sh`** — creates a multi-route Stripe mock (charges, refunds, customers). The shape demonstrates a typical "set up before tests, tear down after" flow.
- **`scripts/reset-state.sh GROUP`** — clears all per-route and per-group state for the named group. Use between test phases when you need a clean slate without recreating routes.
- **`scripts/flaky-mock.sh PATH [EVERY_N]`** — creates a single route that returns 503 on every Nth call. Demonstrates stateful behavior (`ctx.store.incr`) and is the canonical pattern for testing retry logic.
- **`scripts/latency-mock.sh PATH`** — creates a single route whose response latency *grows* with elapsed time since first call (default: +50ms per second, capped at 30s). Demonstrates `host.sleep` and `host.monotonicMs` from ADR-0021. Canonical pattern for reproducing API-gateway cascading-failure modes that depend on response time creeping up toward a timeout threshold.

Read the scripts before running — they're written to be readable as documentation. The handlers are intentionally small; copy and adapt for your own routes.

## Handler API in 30 seconds

The handler exports a single function with three positional parameters: the request, the per-route store, and the per-group store. Both stores expose the same `bucket` interface (get / set / delete / incr / list_push / list_range / hash_*; for the full API surface, run `wm capabilities store` against any reachable host — or `get_capabilities(topic: "store")` if you're driving via MCP).

```ts
export function handle(req, routeStore, groupStore) {
  // req: { method, path, headers: [[k,v]...], body: Uint8Array,
  //        pathParams: [name, value][], query: [name, value][] }
  // routeStore: per-route key-value store, scoped to this route only
  // groupStore: per-group shared store, visible to every route in the group
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode("..."),
  };
}
```

Two binding quirks worth knowing up front, both inherited from the WIT-to-JS conversion:

- **Field names are camelCase**, not snake_case. The WIT contract uses kebab-case (`path-params`, `matched-pattern`); the JS binding produces `pathParams`, `matchedPattern`. Get this wrong and you get a runtime trap that's hard to read.
- **`incr` returns a `bigint`**, not a Number — the WIT type is `s64`. So `routeStore.incr("count", 1n)` and `n % 3n === 0n`. Convert with `Number(n)` before JSON-serializing if you want a plain number in the response.

Persistent state survives between requests until the group expires or you call `wm groups state GROUP --clear`. Use it for counters, last-seen-payload assertions, multi-step flows ("third call returns 503"), or anything else that needs to remember.

The handler also imports a `log` interface (see `wit/wiremirage.wit`) — log lines emitted from a handler attach to the corresponding journal entry and show up in `wm journal show`.

A `host` global exposes three time primitives (ADR-0021):

- **`host.sleep(ms)`** — block the handler for `ms` milliseconds. Counts against the sandbox's per-request wall-clock budget (~30s for JS/TS handlers, ~1s for AOT components); a sleep that exceeds it traps the handler. Use to simulate slow upstream APIs; pair with state to make the delay grow over time (see `scripts/latency-mock.sh`).
- **`host.wallTimeMs()`** — current wall-clock time in milliseconds since the Unix epoch. May jump backwards on NTP correction; use `monotonicMs` for measuring elapsed time.
- **`host.monotonicMs()`** — opaque monotonically non-decreasing counter, milliseconds. Useful only as a *difference* — store the value at T₁, read it again at T₂, subtract. Doesn't reset across requests within the same host process.

## Inspecting what happened

`wm journal list <group>` shows every dispatched request to that group, newest first. `wm journal show <group>/<n>` shows the full entry: request, response, handler logs, timing, errors. The journal has a 1-hour TTL; for longer-lived debugging, pull entries off and store them yourself.

The list commands (`wm routes list`, `wm groups list`, `wm journal list`, `wm unmatched list`) all accept filter / sort / pagination flags — `--method`, `--path-pattern '/v1/*'`, `--status 5xx`, `--since 5m`, `--q stripe`, `--sort last_hit_at --dir desc`, `--limit 20 --offset 40`. The list output includes a `(showing K of N; --offset M for the next page)` footer when paginated. Run `wm <command> --help` for the exact flag set per command.

`wm unmatched list` (admin-only) is the host-wide view of requests that arrived but didn't match any route — reach for it when debugging "my mock isn't firing". Same filter vocabulary minus `route` and `status`.

For host-wide observation, the MCP server exposes two streaming tools — `wait_for_request` (block until N matching entries arrive, with timeout) and `tail_journal` (stream entries until idle or max-entries). Reach for these when a Bash-friendly polling loop would be awkward; agents with MCP access tend to find them more ergonomic than `while true; do wm journal list ...` patterns.

## Gotchas

- **Group TTL.** Default 24h, sliding on every request match. Tests that span more than a day, or non-sliding groups that pause for hours, can have routes vanish from under them. Bump TTL via `wm groups update` or `wm groups refresh`.
- **Route conflicts.** Two routes with overlapping path patterns in the same group cause a conflict at create time. Across groups it's a host-wide conflict — only one mock can claim a given path/method. The error message names the conflicting route.
- **No state reset from inside handlers.** Handlers can read/write their state but can't bulk-clear it. Use `wm groups state --clear` from outside.
- **Ownership.** Routes carry an `owner_id`; non-admin callers can read shared state but only modify their own routes. Admins bypass.
- **Implicit groups.** If you `wm routes add` without `--group`, the host creates a single-route group named `_route_<ulid>`. Useful for one-offs but they don't show up in `wm groups list` unless you ask.
- **Mock traffic is unauthenticated by design.** SUTs don't carry tokens; only the `/__api/*` surface and `/__ui/*` (when present) are gated. Don't put secrets in mock route paths.

## When you need to debug

If you've created a route and your SUT still gets 404, the journal isn't showing what you expect, or state seems wrong, switch to the `wiremirage-debug` skill. It walks through the diagnostic loop: check what's reaching the host, check what would match, inspect the journal record's error field, look at handler logs.

## Where to look for more

- `wm capabilities [topic]` — the full handler API as markdown, fetched live from the connected host. Topics: `overview`, `request`, `response`, `store`, `log`, `clock`, `gotchas`. Always agrees with the host's actual capabilities (the CLI fetches `/__api/capabilities` rather than embedding a static copy). The same content is reachable via the MCP `get_capabilities` tool.
- `wm <command> --help` — the source of truth for command details. Always more current than this skill.
- `wm --help` — the surface map.
- `wm completion bash|zsh|fish|powershell` — emit a completion script for your shell. Pipe it into the appropriate location once.
- The host's admin API at `/__api/*` is what `wm` wraps; if a script needs something the CLI doesn't surface yet, the REST API may have it.
- The MCP server at `/__api/mcp` exposes most operations as tool calls for agents that prefer that surface. (User management is intentionally CLI-only — agents shouldn't be creating users.)
- For admin tasks (managing users, distributing tokens to teammates), `wm users` and `wm tokens` are the primary CLI surfaces.
