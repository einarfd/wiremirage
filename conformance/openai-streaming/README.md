# Conformance: OpenAI streaming chat completions

Black-box check that the **real `openai` Python client** is happy talking to a
WireMirage mock of `POST /v1/chat/completions` — both the streaming (SSE) and
buffered shapes. The client (pydantic models, a real SSE decoder, typed error
mapping) is the validator; if it accepts our bytes, the mock is faithful.

This is a *forcing function*, like TinyGo for the WIT contract: it surfaces the
small things hand-written mocks get subtly wrong (SSE framing, content-type,
error-body shape) that WireMirage's own unit tests can't see.

## Run it

```sh
./run.sh
```

Boots the host in-memory, registers the routes, runs `conformance.py`, tears the
host down. **Opt-in** — not part of `just check`.

Prereqs: a buildable host (Docker, for the js-engine build) and `python3` with
`venv`. The `openai` client is pinned in `requirements.txt` and installed into a
local `.venv` on first run.

## What it pins down

| # | Check |
|---|-------|
| 1 | Streaming chunks assemble to the expected text; `finish_reason == "stop"` |
| 2 | Buffered (`stream=False`) response parses as a `chat.completion` |
| 3 | **Incremental flush** — with 150 ms/token pacing, chunks arrive spread over time, proving ADR-0022 streaming reaches the client live, not buffered |
| 4 | Request-body parameterization via the SDK's own `extra_body=` (`mock_delay_ms`) |
| 5 | An OpenAI-shaped error body surfaces as the right typed SDK exception (`RateLimitError`, 429) |

## Files

- `handler.ts` — the mock served at `/v1/chat/completions` (streaming + buffered, honors `mock_delay_ms`)
- `error-handler.ts` — an OpenAI-shaped 429, served at `/v1-error/chat/completions` (check 5)
- `conformance.py` — the client-side assertions
- `run.sh` — orchestrator

## Notes from building this

- **The `openai` client is lenient on streaming chunks.** Missing `created` /
  `model` on a chunk does *not* raise — the stream path uses lenient
  construction, not strict validation. WireMirage's shipped `streaming-llm-mock`
  example (which omits those) is therefore accepted as-is. `handler.ts` includes
  them anyway to match the real API.
- **Error mapping is by status code + body shape.** Return
  `{"error": {message, type, code, param}}` with the right status and the SDK
  raises the matching typed exception. A handler that simply `throw`s yields the
  engine's generic `text/plain` 500 → `InternalServerError` (still handled, just
  not OpenAI-shaped).
- **`extra_body` is a clean parameterization channel** — the SDK forwards unknown
  keys verbatim into the request body, so a mock can be tuned (pacing, scenarios)
  without any out-of-band config. Relevant to the reusable-mock direction.

## Pin

`openai==1.99.1`. Bump deliberately; a major SDK rev can change SSE handling or
model validation, which is exactly what this lane exists to catch.
