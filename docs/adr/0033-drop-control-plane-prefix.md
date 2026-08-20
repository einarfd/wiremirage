# ADR-0033: Drop the `__` control-plane path prefix — apex-only control-plane routing

**Status:** Accepted

**Supersedes:** the reserved-path scheme in ../route-model.md (the `/__*` prefixes and the global reserved-path check). Builds directly on [0030-virtual-host-routing.md](0030-virtual-host-routing.md).

**Context:**

Every control-plane surface lives under a double-underscore prefix: `/__api` (REST + the `/__api/mcp` MCP endpoint), `/__ui`, `/__auth`, `/__health`, `/__ready`. The prefix exists for exactly one reason: under the **flat namespace** that predated [0030-virtual-host-routing.md](0030-virtual-host-routing.md), control-plane paths and user *mock* routes shared a single global path space, so the host needed a prefix mock routes were vanishingly unlikely to claim (`/__health` collides with nothing real; `/health` would collide constantly).

[0030-virtual-host-routing.md](0030-virtual-host-routing.md) removed that constraint. Mock traffic is now served only on group subdomains (`{group}.{apex}`); the apex is control-plane only and serves no mock traffic. The two namespaces are separated by **host**, not by path prefix — so the `__` no longer disambiguates anything. It is now purely vestigial, and it reads as noise in the browser address bar, in MCP client configs (`…/__api/mcp`), and in probe URLs.

There is a catch that makes this more than a rename. Today the control-plane sub-routers are mounted **by path with no host gating** (`server.rs`: `/__api`, `/__ui`, `/__auth`, `/__health`, `/__ready` are matched on every host; `dispatch` is only the fallback that does Host→group resolution). So `/__*` is served on *every* host, and the global `is_reserved_path` check fires before host resolution. With the `__` prefix that is harmless. But if `/__health` → `/health` and `/__api` → `/api` while routing stays path-global, those bare paths would **shadow mockable paths on every subdomain** — a tenant could no longer mock `GET /health` or `/api/*`, which real systems-under-test absolutely expose. So the rename is only safe if control-plane routing becomes **apex-only**.

That apex-only behaviour is the correct ADR-0030 end-state regardless: the apex is the control plane, a subdomain is pure mock space.

**Decision:**

Two coupled changes, shipped together as one breaking cutover:

- **Drop the `__` prefix on every control-plane path.** `/__api`→`/api`, `/__ui`→`/ui`, `/__auth`→`/auth`, `/__health`→`/health`, `/__ready`→`/ready`, and the MCP endpoint `/__api/mcp`→`/api/mcp`. No prefix replacement — the segment name (`api`, `ui`, …) carries the meaning.
- **Route the control plane by host.** A request to a recognized `{group}.{apex}` subdomain is diverted to mock dispatch *before* any control-plane route can match, so on a subdomain *every* path — `/api/*`, `/health`, `/auth/*`, `/ui/*` — is mockable and can be claimed by a user route. Every other host (the apex itself, plus direct / loopback / bare-IP access) is served the control plane, auth-gated as before — so hitting the box directly by IP in dev still reaches `/api`, `/ui`, etc. The notion of a globally "reserved" path goes away: a group subdomain reserves nothing, and the control plane lives off the subdomains. The bare-`/` redirect (to `/ui/` or `/auth/login`) is unchanged in behaviour, only in target path.

Consequences for the surrounding surfaces:

- **Probes** move to `GET /health` and `GET /ready` on the apex; deployment/orchestrator configs repoint.
- **MCP** mounts at `{apex}/api/mcp`; the OAuth discovery metadata (RFC 9728 / 8414, [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md)) advertises the new path, and the Connect page + `who_am_i`/`summarize_workspace` `base_url` derivation emit it. Existing MCP client configs must be re-pointed.
- **CSRF / auth / session** middleware path scoping (`/ui/*`, `/auth/*`) and the auth-redirect middleware update to the new prefixes.
- **The `wm` CLI and `wm-core` client** ship the new paths in lockstep, so a CLI upgrade is the whole migration for CLI users.
- **One-time breaking cutover**, in the spirit of ADR-0030's: REST callers, MCP client configs, probe URLs, and OAuth metadata consumers all change at once. Acceptable because the deployment is ephemeral-by-design (no backups, see the project's no-backup posture) and pre-1.0, and the operator already absorbed the ADR-0030 cutover.

**Consequences:**

- **Cleaner, less surprising URLs** — `wm.example.com/ui`, `…/api/mcp`, `…/health` — with no vestigial marker. This is the whole motivation.
- **Subdomains can mock previously-blocked common paths** (`/health`, `/api/*`, `/auth/*`) — a real fidelity win for mocking SUTs that expose those exact paths, which the flat-namespace prefix could never allow.
- **Routing becomes host-aware** — a gate diverts recognized group subdomains to mock dispatch; the control plane is served on everything else. This is a genuine routing change (not a find-replace) and the main implementation risk; it wants a test that a subdomain route at `/health` is served as a mock (not shadowed by the apex probe), which is the behaviour that proves the gate fires.
- **Breaking cutover** across REST, the MCP endpoint URL, probe URLs, and OAuth metadata — a second cutover after ADR-0030.
- **Large mechanical sweep**: ~970 `/__` references across the host, templates, CLI, the product skill, conformance lanes, and docs (README, CLAUDE.md, and the Arkiv design docs). `route-model.md`'s reserved-paths section is rewritten to the apex-scoped model.
- The `/__admin/*` reserved prefix (currently listed but only used by the stub admin health page) folds into the same scheme as `/admin/*` on the apex.

**Alternatives considered:**

- **Keep the `__` prefix (status quo).** Rejected: it is vestigial post-ADR-0030, reads as noise on every user-facing URL, and — as a side effect of the global reserved-path check — needlessly blocks subdomains from mocking `/__*` paths. The only thing it buys is avoiding a cutover.
- **Rename only the browser-facing paths (`/__ui`→`/ui`, `/__auth`→`/auth`), leave `/__api`, `/__health`, `/__ready` and the MCP URL.** Rejected: inconsistent (leaves `__` on the most-referenced surface and on the user-visible MCP endpoint), and still requires apex-only routing for the renamed paths — most of the work for half the payoff.
- **Replace `__` with a different marker** (`/_api`, `/-/…`, `/.wm/…`). Rejected: any marker still "looks weird," and apex-only routing makes a marker unnecessary — the host *is* the disambiguator now.
- **Rename without apex-only routing.** Rejected: bare `/health` and `/api/*` would shadow mockable paths on every subdomain, silently breaking the ability to mock SUTs that use those paths. The two changes have to ship together.
- **Keep control-plane reachable on subdomains too (path-global), just renamed.** Rejected: same shadowing problem, and it muddies the ADR-0030 model that a subdomain is pure mock space.

**See also:** [0030-virtual-host-routing.md](0030-virtual-host-routing.md) (the apex/subdomain split this completes), ../route-model.md (reserved-paths section superseded), [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md) (MCP endpoint URL + OAuth discovery metadata change), [0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md) (forwarded-header / base-URL derivation), ../virtual-host-impact.md (per-subsystem impact map), ../web-ui-design.md (UI path references)
