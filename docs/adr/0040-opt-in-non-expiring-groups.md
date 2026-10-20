# ADR-0040: Opt-in non-expiring groups — `ttl_seconds: 0` as "never"

**Status:** Accepted

## Context

TTL is the lifecycle mechanism ([0005-valkey-storage.md](0005-valkey-storage.md)). Groups carry a positive TTL — default 24h, hard max 30d ([0013-groups-first-class.md](0013-groups-first-class.md)); routes and handler state carry no TTL of their own and are reaped by the cascade sweeper when their parent group dies. Journals are the other way around: their entries carry their own 1h TTL, independent of the group's lifetime. The design is deliberately ephemeral end to end: no backups, Valkey volume loss means starting over.

The sanctioned keep-alive story has a gap. Sliding TTL (default on) keeps an **in-use** group alive indefinitely, and `refresh_group_ttl` pushes expiry out manually — but both re-arm against the configured TTL, capped at 30d, and neither helps an **idle** group. A demo mock, a long-lived reference mock, or a "semi-permanent" surface that sits untouched for a week between demos dies today. The only workaround is a cron job calling `refresh` — machinery outside WireMirage for a need that is squarely inside it.

Making groups non-expiring by default is a non-goal: ephemerality is the model. The need is narrow, opt-in, and user-known — a long-lived demo or reference mock.

## Decision

`ttl_seconds: 0` means **never expire**. It is an opt-in sentinel, accepted at group create and patch everywhere TTL is accepted (REST, MCP, CLI, bundle spec, UI form). Everything else about TTL semantics is unchanged.

- **Validation.** `0` becomes legal alongside the existing `1..MAX_GROUP_TTL_SECONDS` range; omitted TTL still means the 24h default. `normalize_ttl` rejects `0` today ("must be positive"), so this loosens an existing rejection rather than claiming an unused value — the conclusion is the same (no group record or wire field ever carried `0`, so no back-compat surface and no wire break), but the change is "allow what was an error", not "claim a free value". The bounds table's "nonzero (must be positive)" min becomes "0 = never, otherwise positive" for groups. The bundle lane round-trips the sentinel: `format_duration(0)` emits `ttl: 0s`, which `parse_duration` reads back as `0`, so an exported spec carries it intact.

- **Error mapping (and a bug this fixes).** TTL validation today doesn't reach `validation_failed`: `normalize_ttl` reports a zero or out-of-range TTL as `RegistryError::Malformed`, which both surfaces map to an **internal** error — REST as "malformed record" (api.rs), MCP the same (mcp/error.rs) — so `ttl_seconds: 9999999` comes back as a 500 today, not a 400. `Malformed` is also the variant for corrupt stored records, so it cannot be remapped wholesale. This change adds a dedicated validation variant to `RegistryError`, maps it to `validation_failed` on both surfaces (the code already exists there), and reports both TTL bounds errors and the refresh rejection below through it. The out-of-range 500 is an existing bug, fixed in the same change or immediately before it; a tier-2 test for an out-of-range TTL pins it.

- **Storage.** The group record stores `ttl_seconds: 0` so surfaces can render "never expires" rather than inferring it. Two keys carry the group's TTL today — the `group:{ulid}` hash and the `group:by-name:{name}` index — and every TTL-arming path must treat `0` as "arm no expiry":
  - Create with `0`: write both keys with no `EXPIRE` at all.
  - Patch positive → `0`: **explicitly clear the TTL on both keys** (`PERSIST`). Skipping the `EXPIRE` call is not enough — the old TTL is already armed and the group would still die on schedule.
  - Rename of a `0`-TTL group: carry the no-expiry state to the new `group:by-name` key instead of re-arming from `ttl_seconds`.
  - Patch `0` → positive: re-arm both keys from the new TTL, as any patch does today.

  The hazard that makes this non-negotiable: Valkey's `EXPIRE key 0` deletes the key immediately, so no code path may pass the stored `0` through to `EXPIRE` — a missed case does not extend a lifetime, it destroys the record or the name index. Implementation shape: every group-TTL arming — create, both patch directions, rename, the sliding bump — routes through **one** function that owns the 0 check, so "no stored 0 reaches `EXPIRE`" is a property of one place, reviewable as such, rather than a convention scattered across call sites. The record joins users and tokens in the "no TTL" pattern of ../storage-model.md. The children are untouched, for two different reasons: routes and `kv:`/`gkv:` state have no native TTL and are only reaped by the cascade when the parent dies — which a never-expiring parent never triggers — while journal and unmatched entries already carry their own 1h TTL regardless of the group's lifetime. The sweeper needs no changes: it polls (routes whose parent group no longer resolves are cascade-deleted), and a never-expiring parent always resolves.

- **Refresh.** `POST /api/groups/{group}/refresh` (and `refresh_group_ttl` / `wm groups refresh`) is **rejected** with `validation_failed` on a 0-TTL group — there is nothing to bump, and a silent no-op would suggest the call did something. REST and MCP already have the code; what's missing is the registry-side validation variant that reaches it (previous bullet). The CLI rides REST and inherits the 400 for free.

- **Sliding.** `sliding_ttl` is a **no-op** on a 0-TTL group, not a rejection. The flag stays stored exactly as configured and the dispatch bump does nothing while `ttl_seconds == 0` — "re-arm expiry to the configured TTL" is vacuous when the key has no expiry. Two reasons this beats fail-fast. First, sliding defaults to `true`, so rejecting the pair would fail the minimal create payload (`ttl_seconds: 0`, nothing else) on a default the caller never chose, forcing every client — agents especially — to learn a `sliding_ttl: false` pairing rule just to spell "never". Second, leaving the stored flag untouched means patching back to a positive TTL restores the previously configured behavior exactly; a rejection-based rule would have burned the flag's value into the caller's retry payload instead. The `0` check in the bump path is load-bearing regardless: the sliding path must never reach `EXPIRE` with a stored `0`. Surfaces may render the flag as "no effect while never-expiring"; the skill notes this.

- **Permissions.** No special gate. `0` is not treated as higher-blast-radius than any other TTL value: creating a 0-TTL group requires exactly what creating any group requires (any authenticated user; the creator owns it), and changing a TTL to `0` goes through the same owner-or-admin patch gate as every other group change ([0011-route-ownership.md](0011-route-ownership.md)). The shared-host trust model already lets any user create groups, routes, and state; `maxmemory` is the real bound, not a per-knob permission.

- **Surfaces.** No group surface carries a computed `expires_at` today — REST, MCP, and UI group shapes expose `ttl_seconds` only — and this ADR adds none: "never expires" renders from the stored `ttl_seconds: 0`. rest-api.md's planned group shape does include a deferred `expires_at`; when that field lands it will be `null` for 0-TTL groups, so it can never be misread. The UI rejects 0 in two places today — the new-group form's `min="1"` input and the edit handler's positive-integer check ("TTL seconds must be a positive integer.") — and both must learn the sentinel; the edit path is the easy one to miss. The UI group form gains a "Never expires" option; the skill and CLI docs note the sentinel and the sliding no-op.

## Consequences

- Long-lived demos and reference mocks work without external refresh machinery — the motivating gap is closed with one sentinel value and a single TTL-arming function that knows it.
- **The "ephemeral by design" stance gains a deliberate, bounded exception.** A 0-TTL group is a permanent resident of the store until manually deleted. This does not amend ephemerality's other legs: no backups, volume loss still loses everything, journal/unmatched entries still age out at their own 1h.
- **Eviction interacts with the sentinel, and not the way an expiry-driven design would.** Eviction does not create permanent orphans: the sweeper polls every 30s and cascade-deletes any route whose parent group no longer resolves, whether the record was dropped by `EXPIRE` or by an evicting policy. What 0-TTL actually changes:
  - Under `volatile-*` policies only keys with a TTL are eviction candidates, and a 0-TTL group arms none — the record is immune to a policy that would reap a TTL'd group.
  - Under `allkeys-*` policies the record can be evicted anyway, and the sweeper then quietly deletes the "permanent" mock within one interval, after a brief 404 window. Non-expiring is not eviction-proof.
  - Under `noeviction` nothing is evicted at all; instead the footprint is never reclaimed, and once the store is full, writes fail for everyone.
  Hence the Helm README requirements stand for deployments where non-expiring groups are in use: `maxmemory` sized for it, `maxmemory-policy` verified `noeviction`. The README note is updated as part of this: its "orphans the routes behind it" line predates the sweeper's reap and should say eviction costs a brief 404 window and then the mock is swept.
- **Cleanup is manual.** Deleting the group (or patching it back to a positive TTL) is the only reaper. The existing delete cascade handles the cleanup path unchanged.
- **Sizing.** A never-expiring group's routes and state are counted against `maxmemory` forever; in practice these are small. There is no operator gate — any user who can create a group can create a permanent one, the same trust model as any other storage-consuming create.
- **rest-api.md updates.** The "TTL persistence" note gains the `group:by-name` key and the positive→0 `PERSIST` transition (it currently names only the `group:{ulid}` hash); the refresh section's "bumps `expires_at`" wording gains the 0-TTL rejection; the planned group shape's deferred `expires_at` is documented as `null` for 0-TTL groups.

## Alternatives considered

- **Raise the max TTL (e.g. to 365d) instead.** Rejected: any finite cap re-creates the same problem one magnitude later, and large caps quietly erode the ephemeral model for everyone. The need is binary — "should this ever die on its own?" — not "how much longer?"
- **Default non-expiring groups.** Rejected: inverts the model. TTL-by-default is what keeps a shared host self-cleaning and keeps `maxmemory` sizing a journal-volume problem. The need is opt-in by construction.
- **A separate `persistent: bool` group field.** Rejected: a second, redundant knob where one already communicates the intent; `0` is unambiguous and reuses every existing surface unchanged.
- **`ttl: "0"` / `"never"` string sentinels.** Rejected for the numeric wire fields — `ttl_seconds` is `u64` across REST/MCP/CLI, and a numeric sentinel avoids a wire-format change there. The bundle spec's `ttl` is already a human duration string parsed by `parse_duration` (`wm-core/src/spec.rs`, the bundle lane of [0031-reusable-mock-bundles.md](0031-reusable-mock-bundles.md)), but it needs no new spelling either: `0` and `0s` parse to the sentinel and `format_duration(0)` emits `0s`, so one rule — a zero duration means never — covers every surface. A `never` keyword would be a second spelling of the same fact. The human-facing spellings ("never", "0") stay a docs/UI concern.
- **Sliding-only ("if you keep using it, it stays") as the answer.** Retained as the default behavior and unchanged — but it cannot serve the idle-week-between-demos case, which is precisely the use case this ADR exists for.
- **Reject `sliding_ttl: true` on a 0-TTL group (fail-fast).** Rejected: deterministic and matches the fail-fast house rule, but sliding defaults to `true`, so it would reject the minimal common payload on a default the caller never chose and force every client to learn a two-field pairing rule. The no-op has no failure mode, keeps the stored flag honest, and round-trips through patch.

## See also

- [0013-groups-first-class.md](0013-groups-first-class.md) — the TTL model this amends — max-bound rule gains a 0 exception
- [0005-valkey-storage.md](0005-valkey-storage.md) — TTL as lifecycle; the "no TTL" pattern this joins for groups
- [0011-route-ownership.md](0011-route-ownership.md) — the owner-or-admin patch gate this rides unchanged
- ../storage-model.md — "TTL defaults and bounds" table: group min becomes "0 = never, otherwise positive"
- ../rest-api.md — the refresh path spelling and the TTL-persistence note this keeps in step; the reserved PTTL-backed `expires_at` future this ADR defers
- `deploy/helm/wiremirage/README.md` (repo) — the noeviction/maxmemory note this makes load-bearing and whose orphan wording this corrects
