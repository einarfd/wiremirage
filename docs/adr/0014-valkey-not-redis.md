# ADR-0014: Valkey, not Redis

**Status:** Accepted

**Context:** [0005-valkey-storage.md](0005-valkey-storage.md) commits WireMirage to a Redis-protocol-compatible storage backend. There are now several:

- **Redis** (the original, by Redis Inc.)
- **Valkey** (the open-source fork from the Linux Foundation)
- **DragonflyDB** (a from-scratch reimplementation in C++)
- **KeyDB** (multi-threaded fork, now owned by Snap)

All speak the Redis wire protocol; all work with the same `redis-rs` Rust client. The choice is essentially one of licensing, governance, and ecosystem trajectory.

**Decision:** Use Valkey as the default backend. The `Storage` trait abstraction allows alternatives, but the WireMirage project's reference deployment, documentation, and Helm/Compose manifests target Valkey.

**Why this matters now:**

In **March 2024**, Redis Inc. relicensed Redis from the BSD 3-Clause license to a dual-license model: RSALv2 (Redis Source Available License) or SSPL (Server Side Public License). Both are non-OSI-approved licenses that restrict commercial cloud-hosting use cases. This change applies to all Redis versions from 7.4 onward.

In response, the Linux Foundation announced **Valkey** in March 2024 as a BSD-licensed fork from Redis 7.2.4 (the last BSD-licensed version). Valkey is governed by the Linux Foundation; major contributors include AWS, Google Cloud, Oracle, and Ericsson. By late 2024, Valkey 8.0 had shipped with substantial improvements including ~3x the performance of Redis 7.2.4 on common workloads (per Linux Foundation announcements; verify current numbers).

Major cloud providers have transitioned: **Amazon ElastiCache**, **Google MemoryStore**, and **Oracle Cloud** all support or default to Valkey. Linux distributions (Ubuntu, Debian, Fedora) have either replaced their `redis` package with Valkey or are in the process. The community trajectory is clearly toward Valkey for OSS use.

**Consequences:**

- **OSS-friendly licensing.** Anyone can run, modify, redistribute, or build commercial products on top of Valkey without license concerns. Important for WireMirage's intended Apache 2.0 distribution.
- **Active development with broad governance.** Linux Foundation governance means no single company controls the roadmap. Compares favorably to a single-vendor project.
- **Wire-protocol compatibility.** Existing Redis clients (`redis-rs`, `redis-py`, etc.) work unchanged. Migration from Redis to Valkey is, for most use cases, just changing the binary.
- **Performance parity or better.** Valkey 8.x is competitive with or exceeds Redis 7.x on common benchmarks. Worth re-verifying when v1 ships.
- **Ecosystem still maturing.** Some third-party tools default to "Redis"; Valkey support is generally added but may lag. Worth checking when picking ancillary tooling (e.g., monitoring, GUIs).
- **Naming awkwardness in code.** `redis-rs` is the client library, configuration uses `redis://` URLs, but the server is Valkey. We document this clearly to avoid confusion.

**Alternatives considered:**

- **Redis Inc.'s licensed version.** Rejected for OSS distribution. Acceptable for users who choose to deploy it themselves (the wire protocol is identical), but not the default.
- **DragonflyDB.** Higher performance for some workloads, BSL license (which has its own constraints similar to Redis Inc.'s situation), but smaller community than Valkey. Source-available, not OSS. Rejected as default for the same reason.
- **KeyDB.** BSD license, multi-threaded. Was a strong contender pre-Valkey; less actively developed since Valkey absorbed most of the community attention. Acceptable but not preferred.
- **Roll our own (e.g., on top of redb).** Considered briefly. The cost of implementing TTL, list/hash/set semantics, and pub/sub correctly is high; the value over using a battle-tested implementation is zero. Rejected.

**For deployers who prefer something else:**

The `Storage` trait abstraction means swapping Valkey for another Redis-protocol-compatible server (Redis Inc., DragonflyDB, KeyDB) is a config change, not code. Swapping for a non-Redis-protocol backend (redb, SQLite) requires implementing the trait but doesn't touch the rest of the system. This is a deliberate property — the WireMirage project takes a position on defaults but doesn't lock users into one path.

See also: [0005-valkey-storage.md](0005-valkey-storage.md), ../storage-model.md.
