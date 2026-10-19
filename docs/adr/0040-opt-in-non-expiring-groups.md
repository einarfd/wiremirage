# ADR-0040: Opt-in non-expiring groups — `ttl_seconds: 0` as "never"

**Status:** Proposed

**Context:**

TTL is the lifecycle mechanism ([0005-valkey-storage.md](0005-valkey-storage.md)). Groups carry a positive TTL — default 24h, hard max 30d ([0013-groups-first-class.md](0013-groups-first-class.md)); routes, handler state, and journals have no TTL of their own and are reaped by the cascade-on-expiry sweeper when their parent group dies. The design is deliberately ephemeral end to end: no backups, Valkey volume loss means starting over.

The sanctioned keep-alive story has a gap. Sliding TTL (default on) keeps an **in-use** group alive indefinitely, and `refresh_group_ttl` pushes expiry out manually — but both re-arm against the configured TTL, capped at 30d, and neither helps an **idle** group. A demo mock, a long-lived reference mock, or a "semi-permanent" surface that sits untouched for a week between demos dies today. The only workaround is a cron job calling `refresh` — machinery outside WireMirage for a need that is squarely inside it.

Making groups non-expiring by default is a non-goal: ephemerality is the model. The need is narrow, opt-in, and user-known — a long-lived demo or reference mock.

**Decision:**

`ttl_seconds: 0` means **never expire**. It is an opt-in sentinel, accepted at group create and patch everywhere TTL is accepted (REST, MCP, CLI, bundle spec, UI form). Everything else about TTL semantics is unchanged.

- **Validation.** `0` is now legal alongside the existing `1..MAX_GROUP_TTL_SECONDS` range; omitted TTL still means the 24h default. The bounds table's "nonzero (must be positive)" min becomes "0 = never, otherwise positive" for groups. `0` is a free value: no group record or wire field ever carried it, so there is no back-compat surface and no wire break.
- **Storage.** The group record stores `ttl_seconds: 0` so surfaces can render "never expires" rather than inferring it. The host **skips the `EXPIRE`** on `group:{ulid}` — the record joins users and tokens in the "no TTL" pattern of ../storage-model.md. Routes, `kv:`/`gkv:` state, and journals are untouched: they have no native TTL today and are only reaped by the cascade when the parent dies, which a never-expiring parent simply never triggers. Sweeper, keyspace notifications, and sliding logic need no changes.
- **Refresh.** `POST /__api/groups/{group}/refresh` (and `refresh_group_ttl` / `wm groups refresh`) is **rejected** on a 0-TTL group — there is nothing to bump, and a silent no-op would suggest the call did something.
- **Sliding.** `sliding_ttl: true` on a 0-TTL group is **rejected** at create/patch: the configuration is self-contradictory and the sliding bump on match would be dead code. (Sliding stays meaningful only for groups with a positive TTL.)
- **Permissions.** Setting TTL to 0 — at create or by patching an existing group to it — is **owner-or-admin** ([0011-route-ownership.md](0011-route-ownership.md)). A never-expiring group is the one configuration that escapes the reaper, so on a shared host it should not be quietly creatable by every member.
- **Surfaces.** `expires_at` renders as `null` (REST/UI) with the stored `ttl_seconds: 0` available for an explicit "never" label. The UI group form gains a "Never expires" option; the skill and CLI docs note the sentinel.

**Consequences:**

- Long-lived demos and reference mocks work without external refresh machinery — the motivating gap is closed with one sentinel value and one skipped `EXPIRE`.
- **The "ephemeral by design" stance gains a deliberate, bounded exception.** A 0-TTL group is a permanent resident of the store until manually deleted. This does not amend ephemerality's other legs: no backups, volume loss still loses everything, journal/unmatched entries still age out at 1h.
- **Eviction becomes a permanent orphan-maker.** Dropping a `group:` record while its routes survive already 404s a working mock (../../deploy/helm/wiremirage/README.md); with `noeviction` that was self-limiting because the data would TTL away eventually. A 0-TTL group makes orphaned routes permanent, so `maxmemory` sizing and the `noeviction` check go from recommended to required for any host allowing this flag. The Helm README note is updated to say so.
- **Cleanup is manual.** Deleting the group (or patching it back to a positive TTL) is the only reaper. The existing delete cascade handles the cleanup path unchanged.
- **Sizing.** A never-expiring group's routes and state are counted against `maxmemory` forever; in practice these are small, and the operator gate bounds how many exist.

**Alternatives Considered:**

- **Raise the max TTL (e.g. to 365d) instead.** Rejected: any finite cap re-creates the same problem one magnitude later, and large caps quietly erode the ephemeral model for everyone. The need is binary — "should this ever die on its own?" — not "how much longer?"
- **Default non-expiring groups.** Rejected: inverts the model. TTL-by-default is what keeps a shared host self-cleaning and keeps `maxmemory` sizing a journal-volume problem. The need is opt-in by construction.
- **A separate `persistent: bool` group field.** Rejected: a second, redundant knob where one already communicates the intent; `0` is unambiguous and reuses every existing surface unchanged.
- **`ttl: "0"` / `"never"` string sentinels in the wire format.** Rejected: `ttl_seconds` is `u64` across REST/MCP/CLI and `parse_duration` handles the string form at the spec layer ([0026-string-first-body-encoding.md](0026-string-first-body-encoding.md)); a numeric sentinel avoids a wire-format change entirely. The human-facing spellings ("never", "0") are a docs/UI concern, not a wire concern.
- **Sliding-only ("if you keep using it, it stays") as the answer.** Retained as the default behavior and unchanged — but it cannot serve the idle-week-between-demos case, which is precisely the use case this ADR exists for.

**See also:** [0013-groups-first-class.md](0013-groups-first-class.md) (the TTL model this amends — max-bound rule gains a 0 exception), [0005-valkey-storage.md](0005-valkey-storage.md) (TTL as lifecycle; the "no TTL" pattern this joins for groups), [0011-route-ownership.md](0011-route-ownership.md) (owner-or-admin precedent), [0025-writable-handler-state.md](0025-writable-handler-state.md) (owner-or-admin gate on a high-blast-radius knob), ../storage-model.md ("TTL defaults and bounds" table — group min becomes "0 = never, otherwise positive"), ../../deploy/helm/wiremirage/README.md (the noeviction/maxmemory note this makes load-bearing)
