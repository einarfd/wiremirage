# ADR-0005: Valkey for the storage backend

**Status:** Accepted

**Supersedes:** ADR-0005 v1 (which proposed redb)

**Context:** WireMirage needs storage for several categories of data: users, sessions, API tokens, groups, routes, handler runtime state, request journal, unmatched-request log. Total volume is small (test fixtures, not production data); write rate is low; the shape is naturally key-value with some richer data types (lists for journals, hashes for records, sets for membership tracking).

Critical requirement: **TTL is the lifecycle mechanism.** Routes expire, sessions expire, journal entries expire, unmatched logs expire. A storage backend with native TTL is meaningfully simpler than one where we implement TTL ourselves.

The original ADR proposed `redb` (an embedded pure-Rust KV store). On reflection, two factors flipped the decision:

1. **The compiler-sidecar architecture** ([0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md)) **already commits us to multi-container deployment.** The "one less container" argument for embedded storage doesn't apply when you're already running two.
2. **Native TTL and rich data types** are exactly what we need. Implementing them on redb is real work for no architectural gain.

**Decision:** Use Valkey (BSD-licensed Redis fork) as the storage backend. The host accesses it via `redis-rs`, which works with Valkey identically. Storage is exposed to the rest of the system via a Rust `Storage` trait, so alternative backends can be added later without touching the layers above.

**Consequences:**

- **Native TTL with millisecond precision.** Every entry that should expire gets `EXPIRE` set at creation. We don't write a TTL-sweeper.
- **Native rich data types.** Lists, hashes, sets all map directly to Valkey commands. Handlers get a richer storage interface than byte-blob KV without us implementing the data structures.
- **Keyspace notifications** are available for cascade-delete patterns (group expires → routes deleted).
- **One more container in the deployment.** Since we're already running compiler sidecars ([0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md)), this is operationally cheap. Valkey is the canonical "one more container" — Helm charts, Compose snippets, and operational knowledge are abundant.
- **Network hop on every storage call.** ~0.1-1ms on loopback or in-cluster. Negligible at our request rates.
- **In-memory data model.** The whole dataset must fit in RAM. Mitigated by aggressive TTL — old data evicts itself. `maxmemory` and `noeviction` policy at deploy time prevent silent data loss if RAM is exceeded.
- **Persistence via RDB snapshots and AOF.** Different durability model than redb's full ACID, but consistent with the project's stated stance: WireMirage isn't in your backup scope. A Valkey crash recovers to the most recent fsync; volume loss means starting over. Acceptable for this tool. See ../storage-model.md.
- **Easier path to scale-out later.** If WireMirage ever needs HA, Valkey Cluster and Sentinel are well-trodden. Embedded storage would have made this much harder.

**Alternatives considered:**

- **redb (original choice).** Pure Rust, ACID, MVCC, in-process. Strong technical fit, but lacks native TTL and rich data types. We'd be reimplementing both. The "single container" advantage doesn't apply given multi-container is already required for the compiler sidecar.
- **SQLite via `rusqlite` / `sqlx`.** Same general profile as redb. Same downsides for our use case.
- **External Redis (commercial).** Same wire protocol as Valkey, same client library, but the March 2024 relicensing to RSALv2/SSPL makes it a poor choice for OSS distribution. See [0014-valkey-not-redis.md](0014-valkey-not-redis.md).
- **DragonflyDB or KeyDB.** Redis-protocol-compatible, work via `redis-rs` without code changes. Could be drop-in replacements if a deployer prefers them. We pick Valkey as the default but the trait abstraction supports any of these.

**The Storage trait abstraction:**

The host's storage layer exposes a Rust trait covering KV operations, list/hash/set operations, TTL management, and keyspace notification subscription. The Valkey implementation is the default; alternative implementations (redb, sqlite) could be added if a need for single-binary deployment emerges. The handler-facing WIT contract (../script-api-wit.md) is unaffected by the backend choice.

This adds ~5% complexity to the host code in exchange for keeping doors open. Worth it.

See also: ../storage-model.md, [0008-handlers-in-storage.md](0008-handlers-in-storage.md), [0014-valkey-not-redis.md](0014-valkey-not-redis.md).
