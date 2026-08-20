# ADR-0026: String-first encoding for request/response bodies

**Status:** Accepted

**Amendment (2026-06-09):** `WireBytes` gained an **input-only `{ "json": <value> }` variant** alongside the bare-string and `{ "base64": … }` forms. String-first remains right for the host's *output*, but on *input* an agent doesn't fully control what its MCP client puts on the wire — a JSON-string argument (e.g. a request `body`) can be "helpfully" parsed into a JSON object by the client, which then matched neither the string nor the `{base64}` variant and errored (`did not match any variant of untagged enum WireBytes`). The `{ "json": … }` variant lets an agent pass a JSON value directly and have the host serialize it to the body's UTF-8 JSON bytes. Surfaced by the 2026-06-09 MCP field report. `from_bytes` still only emits the string / `{base64}` forms, so output is unchanged.

**Context:**

[0025-writable-handler-state.md](0025-writable-handler-state.md) established that byte values crossing a JSON boundary are encoded **string-first** — a UTF-8 string by default, `{ "base64": "<...>" }` for binary, never an array-of-ints — because the values are overwhelmingly text and the primary reader/writer is an agent over MCP, where array-of-ints is token-heavy and base64 is unreadable. It applied that to handler **state** (and dry-run's `kv_overrides`) but **explicitly deferred request/response `body`** to a follow-up, since `body` has a wider surface. This is that follow-up.

Today, request/response bodies cross JSON inconsistently and badly:

- **Journal req/resp bodies** — `RequestEnvelope` / `ResponseEnvelope` (`journal.rs`) serialize `body: Vec<u8>` with serde's default, i.e. **array-of-ints**. These are read over REST (`GET /__api/journal/*`), the CLI, the UI, and — most importantly — **MCP** (`wait_for_request`, `tail_journal`).
- **Unmatched req bodies** — same `RequestEnvelope`, same array-of-ints.
- **Dry-run req/resp bodies** — array-of-ints on REST; the MCP `dry_run_route` tool uses a base64 `body_b64` string.

The journal read over MCP is the **core agent workflow**: trigger the SUT → `wait_for_request` to confirm the call landed → inspect the payload. That payload — almost always JSON — currently comes back as `[123,34,...]`. Bodies live on a small number of **shared types** (`RequestEnvelope` / `ResponseEnvelope`), so REST, MCP, CLI, and UI all read the same encoding; you can't fix one surface without forking the type, which means fixing the type fixes all of them at once.

**Decision:**

Extend ADR-0025's string-first encoding to **all request/response body fields** on the agent/operator JSON surfaces:

- `RequestEnvelope.body` and `ResponseEnvelope.body` (journal + unmatched, host + wm-core mirrors).
- `DryRunRequest.body` / `DryRunResponse.body` (host) and `DryRunBody.body` / `DryRunResult.body` (wm-core); the MCP `dry_run_route` `body_b64` field is **replaced** by the tagged form.

Each becomes the same `string | { "base64": "<...>" }` shape: a JSON string when the bytes are valid UTF-8, else base64.

- **Shared type.** The shape is identical to ADR-0025's `StateValue`. Since it now spans state *and* bodies, the Rust type is renamed to a neutral `WireBytes` (in `crate::state` → likely `crate::wire`), used by both; "StateValue" on a journal body reads wrong. The JSON contract is unchanged from 0025's — only the Rust name moves.
- **Clean break, including stored format.** Journal/unmatched records are *stored* in Valkey as JSON, so the envelope's serde change alters the **stored** format too — records written just before a deploy become unreadable for the remainder of their ≤1h TTL, then it's self-healing. No accept-both shim: the data is ephemeral by design and we're pre-1.0, consistent with the project's "no decorative back-compat" rule. Same clean break ADR-0025 took for `kv_overrides`.
- **Out of scope.** `Route.compiled_wasm` (registry `serde_bytes`) stays bytes/base64 — it's a genuinely-binary internal artifact, not a body, and not part of the public surface (ADR-0023).

**Consequences:**

- **The core agent workflow gets readable, cheap payloads.** `wait_for_request` / `tail_journal` return a request/response body as the actual JSON string instead of an array of ~hundreds of integers — a large token and legibility win on the path agents hit most.
- **One encoding everywhere bytes cross JSON** — state and bodies now agree; the `*_b64`-field and array-of-ints conventions are both gone from the public surface.
- **Binary bodies round-trip losslessly** as `{ "base64": ... }` (image/gzip/protobuf responses, non-UTF-8 uploads). Truncated text bodies that cut mid-codepoint fall to base64 — correct, if occasionally surprising.
- **CLI/UI renderers are unaffected** in spirit — they already render bodies UTF-8-when-clean / "(binary, N bytes)" from the deserialized `Vec<u8>`; they keep doing so, now over a smaller wire payload.
- **Cost: a breaking wire+storage change** across journal / unmatched / dry-run on REST + MCP + CLI + wm-core, and a ≤1h window where pre-deploy journal records fail to read. Bounded by TTL and acceptable pre-1.0.

**Alternatives considered:**

- **Leave bodies as array-of-ints.** Rejected: it's exactly the token-heavy, unreadable encoding ADR-0025 removed for state, on the surface agents read *most*.
- **Migrate only the MCP journal read.** Rejected: the body lives on the shared `RequestEnvelope`/`ResponseEnvelope`, so a per-surface encoding would mean forking the type — more complexity than migrating it once for everyone (who all benefit).
- **Accept-both (lenient) deserialization during a transition.** Rejected: journal data is TTL'd and first-party pre-1.0, so a clean break costs at most a ≤1h read gap and avoids a dual-format shim the convention discourages.
- **A body-specific encoding distinct from state's.** Rejected: the shape is identical; one shared `WireBytes` type is simpler than two parallel ones — which is why the type is renamed rather than duplicated.

**See also:** [0025-writable-handler-state.md](0025-writable-handler-state.md), ../storage-model.md, ../rest-api.md, [0023-source-only-public-handler-input.md](0023-source-only-public-handler-input.md)
