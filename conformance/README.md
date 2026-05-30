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

`run.sh` boots `wm-host` in-memory (native, via cargo), registers each lane's
routes, and runs that lane's client **in Docker** (`--network host`) against the
host. Requirements on the machine: **Docker + jq + a buildable host** — no
per-language toolchain on the host itself (that lives in each lane's image). We
already depend on Docker (the js-engine build), so this adds no new host
dependency. Linux-oriented (`--network host` + the host's loopback bind).

CI: `.github/workflows/conformance.yml` (`workflow_dispatch` only — heavier than
the gating CI, so deliberately manual).

## A lane

A lane is a subdirectory containing:

| File | Role |
|------|------|
| `Dockerfile` | Builds the client image (its language + SDK, pinned). `CMD` runs the test against `$WM_BASE`. |
| `routes.json` | The mock routes to register: `[{ "methods": [...], "path": "...", "source": "handler.ts", "group"?: "..." }]`. Sources are TypeScript handler files in the lane dir. An optional `group` attaches routes to a shared (auto-created) group so they share group state. |
| `<sources>.ts` | The mock handler(s). |
| `setup.sh` (optional) | Run after registration with `(base, token)` — for lane-specific seeding (e.g. POSTing config into a mock route). |
| the test | Whatever the `Dockerfile`'s `CMD` runs (e.g. `conformance.py`, a Go binary), asserting against `WM_BASE`. |
| `README.md` | What the lane validates + findings. |

All lanes run against **one** host instance (booted once per `run.sh`), so keep
route paths distinct across lanes.

## Add a lane

1. `mkdir conformance/<name>/` with the files above.
2. Write `handler.ts` (+ any others) and list them in `routes.json`.
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
