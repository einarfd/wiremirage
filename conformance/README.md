# Conformance lanes

Black-box checks that **real third-party client libraries** are happy talking to
WireMirage mocks. The client SDK is the validator: if the actual `openai` /
`stripe` / … library accepts our bytes, the mock is faithful. A forcing
function — like TinyGo for the WIT contract — that surfaces fidelity gaps (SSE
framing, content-type, error-body shape) the Rust unit suite can't see.

**Opt-in.** Not part of `just check`. Run them when touching dispatch /
streaming / the engine, or on a schedule.

## Run

```sh
just conformance                  # every lane
just conformance openai-streaming # one lane
# or directly:
./conformance/run.sh [lane]
```

`run.sh` boots `wm-host` in-memory (native, via cargo), imports each lane's
group spec (`POST /__api/groups/import` — the same spec round-trip the CLI / MCP
/ UI use), and runs that lane's client **in Docker** (`--network host`) against
the host. Requirements on the machine: **Docker + jq + a buildable host** — no
per-language toolchain on the host itself (that lives in each lane's image). We
already depend on Docker (the js-engine build), so this adds no new host
dependency. Linux-oriented (`--network host` + the host's loopback bind).

Mock traffic is served on the group's subdomain `{group}.{apex}` (ADR-0030
virtual-host routing); the apex (`localhost`) is control-plane only. `run.sh`
points each client's `WM_BASE` at `http://{group}.localhost:PORT` and adds
`--add-host {group}.localhost:127.0.0.1` so the label resolves to loopback
inside the container — no DNS needed; the host derives the group from the `Host`
header.

CI: `.github/workflows/conformance.yml` (`workflow_dispatch` only — heavier than
the gating CI, so deliberately manual).

## A lane

A lane is a subdirectory containing:

| File | Role |
|------|------|
| `Dockerfile` | Builds the client image (its language + SDK, pinned). `CMD` runs the test against `$WM_BASE`. |
| `spec.json` | The lane's **group spec**: `{ "name": "<group>", "routes": [{ "methods": [...], "path": "...", "language"?: "typescript", "source_file": "handler.ts" }] }`. `run.sh` inlines each `source_file` into `source` and imports the whole group in one call. The group `name` doubles as the subdomain the client addresses. |
| `<sources>.ts` | The mock handler(s), referenced by `source_file`. |
| `setup.sh` (optional) | Run after import with `(base, token)` — for lane-specific seeding the routes-only spec can't carry (e.g. `PUT /__api/groups/<group>/state`). |
| the test | Whatever the `Dockerfile`'s `CMD` runs (e.g. `conformance.py`, a Go binary), asserting against `WM_BASE`. |
| `README.md` | What the lane validates + findings. |

All lanes run against **one** host instance (booted once per `run.sh`), but each
lane is its own group/subdomain — its own path namespace — so route paths may
overlap freely across lanes (ADR-0030).

## Add a lane

1. `mkdir conformance/<name>/` with the files above.
2. Write `handler.ts` (+ any others) and reference them from `spec.json` via `source_file`.
3. Write the client test and a `Dockerfile` whose `CMD` runs it against `$WM_BASE`.
4. `just conformance <name>`.

`run.sh` discovers lanes by looking for a `Dockerfile`, so it's purely additive.

## Lanes

- [`openai-streaming/`](openai-streaming/) — the real `openai` Python SDK vs a
  mocked `POST /v1/chat/completions` (streaming + buffered).
- [`s3-slowdown/`](s3-slowdown/) — the real AWS Go SDK vs a **reusable,
  config-driven latency/throttle-injection** mock (S3 `GetObject`); proves the
  SDK auto-retries/recovers from injected `503 SlowDown`. The general form of
  "slow down some but not all of a set of requests".
