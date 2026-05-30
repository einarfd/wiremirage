# Conformance: reusable slowdown/throttle injection (S3, Go SDK)

Two things at once:

1. A **reusable, config-driven latency/fault-injection mock** — the general
   form of "slow down *some but not all* of a set of requests" (e.g. a Vertex
   `(model, region)` selective slowdown). The matching + injection engine is
   API-agnostic; only two small functions shape the bytes for the API mocked.
2. A **conformance check** against the real **AWS Go SDK** (`aws-sdk-go-v2`) —
   a non-LLM API and a very different HTTP stack from the Python OpenAI lane.

## What it pins down

| # | Check | Why it matters |
|---|-------|----------------|
| 1 | A non-matching key is served fast | passthrough — slowdown is *partial* |
| 2 | A "slow" key has injected latency (~800 ms) | latency injection works |
| 3 | A "throttled" key (mock returns `503 SlowDown` for the first 2 attempts) still **succeeds** | the real SDK auto-retries/backs off and **recovers** — "slowdown" is an observable client behavior, not just a delay |
| 4 | The same shape, with retries disabled, **surfaces the error** | confirms it's the SDK's retry that recovers in [3] |

Measured locally: [1] ~3 ms · [2] ~807 ms · [3] ~1.6 s (recovered after backoff) · [4] `503` surfaced.

## The reusable engine

`inject.ts` is the artifact. The generic core — read rules, match a request,
inject latency, throttle-with-recovery — is API-agnostic. Rules live in **group
state** under `inject:rules`:

```jsonc
[
  { "match": { "path_prefix": "/slowbucket/" }, "delay_ms": 800 },
  { "match": { "path_prefix": "/throttlebucket/" }, "throttle_first": 2 }
]
```

`match` also supports `method`, `query` (`{k:v}`), and `header` (`{k:v}`) — so
the Vertex case is just `{ "match": { "path_prefix": "/v1/models/gemini-pro",
"query": { "region": "us-central1" } }, "delay_ms": 3000 }`. To retarget the
engine to another API you keep the core and swap the two API-specific functions
at the bottom of `inject.ts` (`successResponse` / `throttleResponse`).

## How config gets in (a finding)

WireMirage has **no public API to write arbitrary kv/gkv state** — only
GET/DELETE. So a runtime-configurable mock seeds its config **through a mock
route**: `config.ts` (`POST /_inject_rules`) writes the request body into group
state. `setup.sh` POSTs `rules.json` there after registration. This works
cleanly via the public surface, but it's worth noting: a first-class
"set state" API (or a "mock config" concept) would make reusable, parameterized
mocks more ergonomic. Relevant input for that direction.

## Other findings

- **GetObject responses want a checksum.** The SDK logs `WARN Response has no
  supported checksum. Not validating response payload.` — real S3 sends an
  `x-amz-checksum-*` (or `Content-MD5`) header. The mock omits it; the SDK warns
  but proceeds. A fully faithful S3 mock would include one.
- **SigV4 is ignored, and that's fine.** The SDK signs every request; the mock
  doesn't validate the signature (mock traffic is open), and the SDK doesn't
  require anything back. Static dummy credentials + path-style addressing are
  all the client setup needed.
- **Path-matching limit (not exercised here).** Real S3 keys contain `/`
  (`folder/file.txt` → `/{bucket}/folder/file.txt`), which WireMirage's
  single-segment `{key}` param won't match. This lane uses flat keys to stay on
  the happy path; multi-segment keys would need wildcard matching WireMirage
  doesn't have.

## Files

- `inject.ts` — the reusable injection engine + S3 response shapes (`GET /{bucket}/{key}`)
- `config.ts` — seeds `inject:rules` into group state (`POST /_inject_rules`)
- `routes.json` — both routes, in the shared `s3-slowdown` group
- `rules.json` — the rule set, POSTed by `setup.sh`
- `main.go`, `go.mod`, `go.sum` — the Go SDK client + pinned deps
- `Dockerfile` — `golang:1.24`, builds + runs the client

Run: `just conformance s3-slowdown` (or `./conformance/run.sh s3-slowdown`).
