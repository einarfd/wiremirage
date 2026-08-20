# ADR-0025: Writable handler state — seed and snapshot via the external state API

**Status:** Accepted

**Context:**

WireMirage exposes per-route (`kv:`) and per-group (`gkv:`) handler state through an *external* state API — operations a test runner or agent performs over REST, not from inside a handler (see ../storage-model.md#external-state-reset-operations). Today that family is **read** and **clear** only:

- `GET /__api/routes/{group}/{n}/state`, `GET /__api/groups/{group}/state` — list keys with kind and a value-or-size preview.
- `DELETE /__api/routes/{group}/{n}/state`, `DELETE /__api/groups/{group}/state` — clear.

There is no way to **write** state from outside a handler. The only code with write access to `kv:` / `gkv:` is a running handler. This gap surfaced concretely while building reusable mocks (the `conformance/s3-slowdown` lane): a config-driven mock — "slow down some but not all `(model, region)` combinations", with the rules supplied as data — has nowhere to receive its config except by POSTing to a **dedicated config mock route** whose handler calls `group-store.set(...)`. That works, but every reusable mock then reinvents a seeding route, the config isn't discoverable, and it couples configuration to a bespoke handler.

[Dry-run](0016-ai-friendly-identifiers.md) already has the inverse need solved internally: `dry_run_route` seeds `kv_overrides` / `gkv_overrides` (`HashMap<String, Vec<u8>>`) into a disposable namespace before running the handler. The encoding and shape exist; they just aren't available against *real* state.

This ADR is deliberately scoped to the **state primitives**. A richer "reusable mock bundle" (a group's routes + initial state + a manifest of tunable knobs, applyable as a unit — "a skill for a group") is a larger design that builds on these primitives; it is explicitly **deferred** until the primitives are in use and have taught us what the bundle actually needs. Group *route* export (`GET /__api/groups/{group}/export`, anticipated in [0008-handlers-in-storage.md](0008-handlers-in-storage.md)) is likewise out of scope here — this ADR is about *state*, not route definitions.

**Decision:**

Add the **write** (and full **snapshot**) operations to the external state family, completing its CRUD. Reset stays a composition (`clear` + `write`), not a new primitive.

1. **Write (upsert).**
   - `PUT /__api/routes/{group}/{n}/state` — set keys in the route's own `kv:` namespace.
   - `PUT /__api/groups/{group}/state` — set keys in the group's shared `gkv:` namespace.
   - Body: `{ "entries": { "<key>": <value>, ... } }`, where each `<value>` is **either a JSON string (stored as its UTF-8 bytes) or an object `{ "base64": "<...>" }` for binary**. Mock state is overwhelmingly text (JSON config, rule sets, templates), and the primary state-writer is an agent over MCP, so a bare UTF-8 string is both the most token-efficient and the most readable encoding; base64 is the escape hatch for genuinely-binary values. (Deliberately *not* the array-of-ints encoding the `body` surface uses — see Alternatives; dry-run's `kv_overrides` is *migrated* to this same form, point 7.) **Upsert semantics**: listed keys are written, others left untouched. A full replace is `DELETE` then `PUT` (i.e., reset).
   - Per-key value capped at the existing `handler_value_size` limit (**1 MiB**, ../storage-model.md#size-limits); oversize is rejected with `validation_failed`.

2. **Snapshot (full-value read).** `GET .../state` keeps its current preview shape (UI-oriented, value-or-size). A snapshot representation returning **full** values in the same tagged shape `PUT` accepts — a JSON string when the stored bytes are valid UTF-8, else `{ "base64": "<...>" }` — is added behind `?format=snapshot`, so export → import round-trips losslessly. This is the existing list view's kind-aware, UTF-8-when-clean rendering (slice 27), just untruncated and round-trippable.

3. **Reset = clear + write.** No new endpoint. Documented as the pattern; the CLI/MCP may offer a convenience that does both, given a snapshot.

4. **Scope: bytes-valued `kv` / `gkv` only.** Lists, hashes, and sets are *not* writable from outside in this ADR — consistent with dry-run, which also seeds bytes-kv only. Deferred until there's a demonstrated need.

5. **Surfaces (parity, per [0015-cli-skill-primary-mcp-secondary.md](0015-cli-skill-primary-mcp-secondary.md)).**
   - CLI: `wm routes state --set KEY=VALUE` (repeatable, UTF-8 bytes), `wm groups state --set KEY=VALUE`; `--snapshot` to dump full values; `--reset-from FILE` = clear + set from a snapshot.
   - MCP: `set_route_state` / `set_group_state`, using the same string-or-`{base64}` value form as REST (not the `_b64`-suffixed-field convention — string-first is the point on the agent surface).

6. **Authorization.** Owner-or-admin, mirroring the existing read/clear state endpoints.

7. **Dry-run seed-state adopts the same encoding.** `dry_run_route`'s `kv_overrides` / `gkv_overrides` (today: REST array-of-ints, MCP `kv_overrides_b64`) migrate to the identical `string | { "base64": "<...>" }` form — seeding disposable dry-run state is the *same operation* as seeding real state, so the two must not ship divergent encodings. This is a **breaking change** to the dry-run surface, taken as a **clean break** (no accept-both shim): WireMirage is pre-1.0, ephemeral by design, and all clients are first-party, so the cost is one coordinated update now versus a permanent — and token-heavy, on the agent surface — inconsistency. The CLI's `wm routes test --kv KEY=VALUE` is unchanged (already UTF-8). The request/response **`body`** encoding is explicitly **out of scope** here: its wider surface (dry-run request, response rendering, journal read across REST/MCP/CLI) warrants its own deliberate decision in a follow-up ADR.

**Consequences:**

- **The config-via-mock-route hack goes away.** A reusable, config-driven mock receives its config through the state API directly; `conformance/s3-slowdown/config.ts` becomes unnecessary. Config is just state keys the mock reads — no second store, no bespoke seeding handler.
- **State CRUD is complete** (was read + clear; now + write + snapshot), and symmetric with dry-run's seeding (same value encoding).
- **Reset between test phases gets a first-class story.** ../storage-model.md already frames reset as an external operation; "reset to a known baseline" is now expressible (clear + write a saved snapshot) rather than only "clear to empty".
- **Unblocks the reusable-mock direction without committing to its format.** Seeding initial state / config is the load-bearing primitive a bundle would need; we ship it standalone and decide the bundle later.
- **Writing real state bypasses the handler.** That is the point and is consistent with the existing external clear — state is plain KV; seeding it is a test-setup action, not handler logic. TTL still applies (state is reaped with the group), so seeded state is as ephemeral as everything else.
- **Cost: another way to mutate state.** The 1 MiB per-key cap and owner-or-admin gate bound the blast radius; the namespaces (`kv:{group}:{route}:*`, `gkv:{group}:*`) are unchanged, so seeded keys live and expire exactly like handler-written ones.

**Alternatives considered:**

- **Keep seeding through a mock route (status quo).** Rejected: every reusable mock reinvents a config endpoint, config isn't discoverable, and it conflates configuration with handler code. The s3 lane proved this is awkward.
- **A separate `config:` store distinct from `kv:` / `gkv:`.** Rejected: a second namespace with its own lifecycle and API for no real gain. Config is just state keys; one namespace keeps the model simple and lets handlers read config exactly as they read any state.
- **Handler-callable `clear`/`seed`.** Rejected, consistent with ../storage-model.md: a handler mutating its own store wholesale mid-flight is a footgun, and the legitimate "reset between phases" case is naturally the test runner's, driven externally.
- **Ship the full reusable-mock bundle now** (routes + initial state + knob manifest, applyable as a unit). Deferred by choice: the primitives are the low-risk keystone and should land first; the bundle format is a larger decision that the primitives will inform.
- **Make `GET .../state` always return full values.** Rejected: the preview shape exists so the UI doesn't render multi-MiB blobs; a `?format=snapshot` opt-in preserves that while enabling round-trip export.
- **Array-of-ints (or base64-only) values**, matching the `body` / `kv_overrides` encoding. Rejected: state values are almost always text (JSON config, rule sets, templates), and the primary state-writer is an agent over MCP, where array-of-ints is markedly token-inefficient and base64 is unreadable. A bare UTF-8 string is both cheaper and clearer; base64 stays only as the binary escape hatch. The `body`/`kv_overrides` surfaces keep their existing encoding (out of scope here), so this surface diverges deliberately rather than propagating a token-heavy convention to a new, agent-facing API.

**See also:** ../storage-model.md, [0008-handlers-in-storage.md](0008-handlers-in-storage.md), [0013-groups-first-class.md](0013-groups-first-class.md), [0015-cli-skill-primary-mcp-secondary.md](0015-cli-skill-primary-mcp-secondary.md), ../script-api-wit.md
