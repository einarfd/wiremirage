# ADR-0016: AI-friendly identifier scheme

**Status:** Accepted

**Context:** Earlier ADRs and design docs (../route-model.md, ../storage-model.md, ../rest-api.md) initially used ULIDs as the universal external identifier across all entity types. ULIDs are good for storage — sortable by time, unambiguous, URL-safe — but they are poor as agent-facing identifiers:

- **Token-inefficient.** A ULID is 26 base32 characters. cl100k-style tokenizers split it into 6-9 tokens per ID because the character distribution looks random. In conversations referencing many IDs, this cost adds up.
- **Visually indistinguishable.** ULIDs sharing a creation minute share their first 10 characters (the timestamp portion). A list of recently-created routes shows ten near-identical strings; agents and humans can't glance-distinguish them.
- **Copy-paste fragile.** When an agent constructs an identifier from earlier context, near-random base32 strings are easy to transcribe wrong (`01HK4PXY9` vs `01HK4PXY2`). Errors compound when the agent then makes follow-up tool calls referencing the wrong route.
- **No semantic anchor.** A ULID doesn't tell you what kind of thing it identifies, what group it belongs to, or what its purpose is.

The right answer isn't "find one identifier format that's good for everything." The right answer is **different entity types use different identifiers tuned to their access patterns**. This is what mature APIs (GitHub, Linear, Stripe) actually do — they use different ID formats for different things, and each format fits its use case.

Researching the state of the art surfaced no academic consensus on AI-friendly identifiers. The closest things are:

- The slug-plus-internal-ID pattern (well-established practitioner consensus, predates LLMs)
- The Proquints proposal (2009, niche but interesting for high-entropy cases)
- Word-based IDs (industry practice for human-friendly handles, e.g., Heroku app names)
- Tokenizer-aware token selection (theoretically possible, practically fragile because tokenizers vary across model versions and providers)

For WireMirage specifically, we have natural parent-scope structure (routes within groups, journal entries within groups), which makes per-parent sequential numbering both possible and the right fit.

**Decision:** Use a mixed identifier scheme tuned per entity type. Internal storage uses ULIDs throughout for time-sortability and primary-key stability; external identifiers vary by entity type:

| Entity | Internal | External (canonical) |
|---|---|---|
| Group | ULID | `name` (e.g., `stripe-mock`) |
| Route | ULID | `{group}/{n}` (e.g., `stripe-mock/7`) |
| Journal entry | ULID | `{group}/journal/{n}` (e.g., `stripe-mock/journal/47`) |
| Unmatched-request entry | ULID | `unmatched/{n}` (host-wide sequential) |
| Token | ULID | `name` (e.g., `Claude Code on laptop`) |
| User | ULID | `{provider}:{email}` (e.g., `google:einar@kindly.ai`), or `me` for self |
| Session | ULID | not externally referenced |
| Trace ID | W3C format | W3C format (unchanged) |

Sequential numbers are assigned by the host on creation by atomically incrementing a per-parent counter (`HINCRBY group:counters:{ulid} next_route_number`). Numbers are **never reused** even after deletion — a deleted route's number stays gone, and a journal entry referencing "stripe-mock/7" continues to mean that specific historical route. This matches GitHub issue numbers and Linear ticket numbers.

The REST API and CLI accept both forms — the scoped slug is canonical (it's what shows up in responses, error messages, audit logs, journal entries), the ULID is an escape hatch for cases where the slug is ambiguous (cross-renaming) or the parent has been renamed mid-call.

**Consequences:**

- **Token efficiency improves substantially.** `stripe-mock/7` is 4-5 tokens; `01HK4P9TYJBM2X1Q3R5Z7VW8DA` is 6-9. Across a conversation referencing 20-30 IDs, this is meaningful but not dramatic — the larger win is qualitative.
- **Agents make fewer transcription errors.** Short, semantic, scoped identifiers are robust to copy-paste; near-random strings aren't. This is the most important property for agent-driven workflows.
- **Identifiers are human-friendly too.** "Route stripe-mock/7" is what a developer says out loud, types into Slack, references in a code review comment. Humans benefit from the same properties agents do.
- **Each format fits its access pattern.** Groups and tokens are user-named at creation; slugs follow naturally. Routes and journal entries belong to a parent and are most naturally numbered within that scope. Users have an existing identity primitive (`{provider}:{email}` matches the admin allow-list config).
- **Internal ULID + external slug is operationally sound.** ULIDs remain the storage primary key, supporting time-sortable cursor pagination and keyspace notifications without change. The translation happens at the API boundary, costing one Valkey hit per request that uses the slug form.
- **Cost: an extra index per scoped-slug entity.** `route:by-number:{group_ulid}:{n} -> route_ulid`, `journal:by-number:{group_ulid}:{n} -> journal_ulid`, `unmatched:by-number:{n} -> unmatched_ulid`. These are small string entries; cheap to maintain.
- **Cost: per-parent counters need to live in storage.** A `group:counters:{ulid}` hash holds `next_route_number` and `next_journal_number` per group. Atomic `HINCRBY` makes assignment race-free across hosts.
- **Cost: numbers can't be predicted before creation.** A client can't say "I'm going to create route 8" — they create the route and the server tells them what number it got. This is fine because creation is server-side anyway.
- **Cost: routes inherit their group's name in the canonical identifier.** Renaming a group renames its routes' canonical paths — `stripe-mock/7` becomes `stripe-mock-v2/7`. The route's ULID is the stable handle for cross-rename references; cached client-side route identifiers may go stale on rename. We accept this; group renames are uncommon and the workaround (use ULID, or refetch) is straightforward.

**Alternatives considered:**

- **Pure ULIDs everywhere.** What we started with. Rejected as the primary external identifier because of the token-efficiency, distinguishability, and transcription-error costs above. Kept as the internal primary key.
- **NanoID** (e.g., `V1StGXR8_Z5jdHi6B`). Marginally better than ULID for token efficiency (fewer chars), still poor for distinguishability and transcription robustness. Doesn't solve the fundamental problem.
- **Word-based IDs** (e.g., `swift-amber-fox`). Excellent for distinguishability and transcription, but entropy is too low for an unguessable production-grade identifier (3 words = 36 bits; 4-5 words to reach 60 bits). Better for human-friendly handles than for unique IDs.
- **Stripe-style prefixed random** (e.g., `route_3PqxAB2eZvKYlo2C0Lh4F8Lw`). The prefix announces type but the suffix is still random. Loses the parent-scope semantics that make `stripe-mock/7` instantly meaningful.
- **Proquints** (e.g., `lusab-babad-gutih-tugad`). Pronounceable pseudo-words with high entropy. Tokenizes better than random base32. Niche; unfamiliar to most developers. Considered but rejected as too unusual for the relatively small benefit.
- **Tokenizer-aware identifiers** that pick characters/sequences expected to be in a tokenizer's BPE vocabulary. Marginally more efficient but tokenizer-specific (cl100k vs o200k vs Claude vs Llama all differ), brittle across model versions, and the gain is small. Rejected.
- **Same numbering scheme for users, sessions, etc.** Considered. Rejected because users don't have a natural parent scope (they're top-level), tokens are already user-named, sessions aren't externally referenced. Forcing all entities into the same scheme would lose the access-pattern fit.

**Implementation notes:**

The by-number index is maintained at creation: when `route:{ulid}` is written, `route:by-number:{group_ulid}:{n}` is also written in the same transaction. On deletion, both are removed. The by-number lookup is a single `GET` returning the ULID, then the route record is fetched normally — sub-millisecond on a same-network deployment.

Group renaming requires updating the `group:by-name` index but does **not** require updating the by-number indices (they key on group ULID, not name). So a rename is cheap; cached client-side identifiers are the only thing affected.

For the unmatched-request log, the host maintains a global `unmatched:counter` integer incremented atomically. The `unmatched/{n}` form is host-wide rather than per-group because unmatched traffic by definition has no group affiliation.

See also: ../route-model.md, ../storage-model.md, ../rest-api.md, ../cli-design.md, ../mcp-surface.md, [0008-handlers-in-storage.md](0008-handlers-in-storage.md).
