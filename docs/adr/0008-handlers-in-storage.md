# ADR-0008: Routes (source and compiled) live in the storage backend, not on disk

**Status:** Accepted

**Supersedes:** ADR-0008 v1 (which proposed on-disk handler files)

**Amendments:**

- *2026-05-23 — interpreted-language path no longer uses a sidecar.* Per [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md), TypeScript / JavaScript routes store source bytes directly (TS transpiled by pure-Rust swc in-host at create / patch time); dispatch goes through an embedded `js-engine.wasm` shared component. References below to "the compiler sidecar re-compiles on source change" and to "Compiler sidecar's container image" apply only to AOT-language compilers (TinyGo, Rust, Zig) when those land. The storage-side decision in this ADR — that route source, artefacts, metadata, and ownership all live in the storage backend — is unchanged; only the *producer* of the wasm bytes for interpreted languages changed.

**Context:** WireMirage handlers are scripts. Where they live — on disk in a directory tree the host watches, or inside the storage backend ([Valkey](0005-valkey-storage.md)) — affects how the system behaves at every level.

The original ADR proposed on-disk storage with the rationale that "handlers should be tracked in Git like any other code." On reflection, that framing is wrong for what WireMirage actually is: it's not a code-hosting service for mocks, it's a running mock service. Teams that want their mocks in Git should use whatever automation creates routes (a setup script, a fixture loader, a developer's tooling) to do that — but Git-as-source-of-truth is *external* to the WireMirage server, not an architectural feature of it.

Two related changes pushed this further:

1. **Routes are explicitly ephemeral with TTL** (../route-model.md). Routes that "live as long as a Git history" doesn't fit a model where every route has a 24-hour expiry by default.
2. **Multi-user authentication and ownership** ([0011-route-ownership.md](0011-route-ownership.md)) makes "Git" the wrong abstraction. A shared Git repo of mock handlers across a team is a coordination nightmare; a shared Valkey-backed service with owner-write-others-read is the right shape.

**Decision:** All route data — source code, compiled Wasm, metadata, owner, group membership, expiry — lives in the storage backend. Nothing about routes touches the host's filesystem.

The only filesystem state the host has is its own configuration (read at startup) and any local caches that are pure performance optimizations (and would be repopulated on demand).

**Consequences:**

- **Routes have a lifecycle, not a Git history.** This matches the project's stance: WireMirage manages running mocks, not a corpus of mock definitions.
- **TTL applies uniformly.** Routes, runtime state, journal entries, unmatched logs all live in one place with consistent expiry semantics. See ../storage-model.md.
- **Compiled Wasm is cached in the same place as the source.** The `compiled_wasm` field on the route record holds the compiled artifact. When source changes, the host re-invokes the compiler sidecar and updates the field. No "filesystem cache directory" to manage.
- **Single source of truth.** "What routes exist" is `SCAN route:*` against Valkey. No filesystem-vs-database divergence to reconcile.
- **Multi-host deployment is conceptually possible** (though not v1). Multiple WireMirage hosts could share a single Valkey instance and serve traffic in parallel without filesystem coordination concerns. We don't actively support this in v1, but we don't preclude it either.
- **Cost: routes are not directly browsable as files.** Engineers can't `cat handlers/charges.ts`. The web UI ([0009-html-htmx-ui.md](0009-html-htmx-ui.md)) handles inspection; for export, the REST API supports `GET /__api/groups/{group}/export` returning a JSON or YAML bundle of group + routes that can be saved or version-controlled externally.
- **Cost: bigger blob storage in Valkey.** Compiled Wasm modules are ~50-300 KB each. At 1000 routes that's ~100-300 MB in Valkey, well within reasonable memory limits but worth knowing.

**Boundary clarification — what's in Valkey:**

- User records and identity bindings
- Sessions and API tokens
- Groups (the lifecycle units; see [0013-groups-first-class.md](0013-groups-first-class.md))
- Routes (source code, compiled Wasm, config, owner, group membership)
- Per-route handler state (the `store` interface)
- Per-group shared state (the `group-store` interface)
- Request journal entries
- Unmatched-request log entries
- Per-parent counters (`group:counters:{ulid}`) for assigning route and journal numbers
- Lookup indices (`group:by-name`, `route:by-number`, `journal:by-number`, `unmatched:by-number`, `user:by-identity`, etc.) — translation between external slugs and internal ULIDs. See [0016-ai-friendly-identifiers.md](0016-ai-friendly-identifiers.md).

**Boundary clarification — what's on the host filesystem:**

- The host binary and its config file (read at startup)
- Compiler sidecar's container image (managed by Docker/Kubernetes)
- That's it. No persistent state under host control.

**Alternatives considered:**

- **Source on disk in Git.** The original v1 proposal. Rejected for the reasons in the context above.
- **Source on disk but ephemeral (host-managed, gitignored).** Compromise position: filesystem caching with no Git semantics. Rejected because once you remove the Git story, the filesystem adds nothing — it's strictly worse than the storage backend on every dimension (TTL, multi-user, lifecycle, queryability).
- **Compiled Wasm on disk, source in Valkey.** Coherent split (artifacts on filesystem, metadata in DB). Rejected as more moving parts than it's worth; co-locating compiled artifacts with their source records is simpler and just as fast given Valkey's read performance.

**Migration path for users who want Git:**

Users who want their mocks in Git use a separate workflow: a script that reads a directory of mock definitions and uses the WireMirage REST API to create routes from them. That script is outside WireMirage's scope; it could be a Bash one-liner with `curl`, a Python helper, or a CI step. The host doesn't care where routes come from, only that they're created via the API.

See also: [0005-valkey-storage.md](0005-valkey-storage.md), ../route-model.md, ../storage-model.md, ../architecture-overview.md.
