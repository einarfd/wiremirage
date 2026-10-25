# ADR-0041: Owner-or-admin reads in a multi-tenant host

**Status:** Accepted

Supersedes the read half of [0011-route-ownership.md](0011-route-ownership.md). Owner-write stays as decided there.

## Context

[0011-route-ownership.md](0011-route-ownership.md) chose **owner-write, others-read**: any authenticated user can list and view any group, route, or journal; only the owner (or an admin) can change them. Its premise was "a small team of engineers" on "a team-internal mock service". It accepted the visibility leak, handler source included, as the price of cross-team learning. It also put privacy out of scope: "If they really need privacy, they run a separate WireMirage instance. We don't bring tenant-style isolation into v1."

[0030-virtual-host-routing.md](0030-virtual-host-routing.md) removed that premise. The single deployment "is meant to be genuinely multi-tenant", each group gets its own subdomain, and "Separate deployment per tenant" is explicitly rejected as the multi-tenant answer. That is exactly the escape hatch 0011 relied on. ADR-0030 never revisited 0011's read policy, but the impact work done for it (../virtual-host-impact.md) already behaved as if reads were owner-scoped: route and group lists stay "host-wide for admin, owned for others", and the unmatched pile went from admin-only to owner-of-the-group visible.

The code followed that direction one handler at a time, without a decision record. By the time of this ADR:

- Route and group lists (REST, MCP, UI) pin non-admins to their own records and reject the `owner_id` filter for them.
- Route source, route state, group state, group show, export, match probe, and unmatched are all owner-or-admin.
- Journal, callbacks, and the live tail use a different test: admin, or the caller owns *a route in* the group. Every other group-scoped read checks the group's owner.
- REST `GET /api/routes/{group}/{n}` was the one read left open (PR #78, a security-audit IDOR finding). MCP `show_route` and the UI already gated the same record.

Meanwhile the design docs still say reads are open: [0011-route-ownership.md](0011-route-ownership.md), ../rest-api.md ("Read endpoints are accessible to any authenticated user"), ../auth-and-authz.md, and ../mcp-surface.md ("Owner-write-others-read applies the same way"; `find_route` described as cross-group). So the decision record and the code disagree, and the one open endpoint looks like a bug or the intended policy depending on which you read.

The cost 0011 accepted has also grown. With a public OIDC issuer, "authenticated" can mean anyone on the internet (see `docs/configuration.md`). Journals capture whatever the system under test sends, which can include real tokens and payloads. On a multi-tenant host, others-read means every account can read every tenant's handler source and traffic.

## Decision

**Reads are owner-or-admin.** A non-admin caller can read a group, route, or anything under them only if they own it. Admins can read everything. This applies to every surface (REST, MCP, UI, and the CLI, which rides REST), so the rule is the same whichever one an agent picks.

Ownership is checked against the record the read is about:

| Read | Rule |
|---|---|
| Route list (`GET /api/routes`, `list_routes`, UI) | Non-admin: own routes only; `owner_id` filter is admin-only. Admin: all, filterable by owner. |
| Route show (`GET /api/routes/{g}/{n}`, `show_route`, UI) | Route owner or admin. |
| Route source / state (REST, MCP, UI) | Route owner or admin. |
| Dry-run (`POST …/dry-run`, `dry_run_route`, UI) | Route owner or admin (it reads source and state). |
| Group list (`GET /api/groups`, `list_groups`, `summarize_workspace`, UI) | Non-admin: own groups only. Admin: all. |
| Group show / state / export (REST, MCP, UI) | Group owner or admin. |
| Match probe (`GET /api/match`, `find_route`) | Group owner or admin. Probing reveals routes. |
| Journal and callbacks (REST, MCP, UI) | Group owner or admin. |
| Live tail (`GET /api/journal/tail`, `tail_journal`, `wait_for_request`, UI) | With a group or route filter: group owner or admin. Without: admin only. |
| Unmatched (`GET /api/unmatched`, `list_recent_unmatched`, `show_unmatched`, UI) | Non-admin: their own groups' entries. Admin: all. |

**One ownership test per scope.** Group-scoped reads, including the journal, callbacks, and the live tail, check the group's owner. Route-scoped reads check the route's owner. This replaces the journal's "owns a route in this group" test, which was an approximation of group ownership. Creating a route in a group already requires owning it ([0011-route-ownership.md](0011-route-ownership.md)), so the two agree in every normal case. The only visible difference is that a group owner who has deleted all their routes can still read the group's remaining journal entries, which is the correct answer.

Reads that are not about tenant data stay open to any authenticated user: capabilities, `who_am_i` / `GET /api/users/me`, and the caller's own tokens. User management stays admin-only as before.

**Not found vs forbidden.** A reference that resolves to nothing returns **404**. A reference that resolves to a record the caller may not read returns **403**. This is uniform across surfaces and is what every gated handler already does (lookup, then gate). It does reveal whether a slug exists, and that is accepted on purpose:

- Group names are already observable without any account: mock traffic on `{group}.{apex}` is unauthenticated, so an existing group's subdomain behaves differently from a missing one.
- Route numbers are sequential per group, so knowing route 3 exists says little once you know the group does.
- A 403 is the actionable answer for an agent: "this exists, you are not its owner" leads to asking the owner or an admin. A 404 would send it looking for a typo. The UI and skill docs already explain 403 as "not owner-or-admin".

Hiding existence would mean returning 404 for both cases on every surface, and that is not worth it while group names are public via DNS.

**No sharing mechanism now.** There is no opt-in "readable by everyone" flag on groups, no teams, and no per-route ACL. The cross-team-learning use case 0011 valued is real, but nobody has asked for it on this host, and the project does not build features that aren't in use. If the ask shows up, it gets its own ADR. The obvious shape is an explicit per-group flag, set by the owner, that widens reads only.

## Consequences

- **The decision record and the code converge.** Most of this is already implemented. Two changes remain: gating REST `GET /api/routes/{group}/{n}` (PR #78), and switching the journal, callback, and live-tail checks to the group owner.
- **Tenants are private by default.** Handler source, state, and captured traffic are visible only to their owner and admins. That is the right default for a host where "authenticated" may mean "has a Google account".
- **Cross-team browsing is gone.** Colleagues on the same host can't see each other's mocks unless one is an admin or they share an account (0011's "service account" workaround still applies). Accepted until someone asks for sharing.
- **Existence is observable, by design.** 404 vs 403 tells a caller whether a slug exists. See the Decision for why this is accepted.
- **Docs to update with this ADR:**
  - [0011-route-ownership.md](0011-route-ownership.md): a status line and note pointing here. The body stays as the historical record.
  - ../rest-api.md: the "Read endpoints are accessible to any authenticated user" convention, and the export and match-probe sections that still say "any authenticated user (read)". The `mine` / `owner_id` filter docs already describe owned-only lists for non-admins.
  - ../auth-and-authz.md: the "Read … any authenticated user" bullet.
  - ../mcp-surface.md: "Owner-write-others-read applies the same way", the `find_route` note calling cross-group visibility intentional (the tool now takes a group and is owner-or-admin), and the journal tools' "owns a route in this group" wording.
  - [0012-api-tokens.md](0012-api-tokens.md): a note pointing here. Its token-compromise bullet puts the blast radius at "the user's own routes plus the read-all view of everyone else's"; under this ADR it is the user's own routes only.
  - ../architecture-overview.md: "The default authorization policy is owner-write, others-read."
  - ../web-ui-design.md: the resolved question about showing other users' route source, answered from the read-all policy.

## Alternatives considered

- **Keep owner-write, others-read (0011 as written) and reopen the gated reads.** Rejected. It fits a small trusted team, not the multi-tenant host ADR-0030 committed to. It would expose every tenant's source and captured traffic to any account, and it would undo several shipped gates.
- **Others-read for metadata, owner-only for source, state, and journal.** Rejected. It splits one record into two privacy levels for little gain: route metadata (paths, methods, URLs, hit counts) already says what a tenant is mocking and how often. It is also a harder rule to state and keep consistent across four surfaces.
- **An opt-in shared or public flag per group, now.** Deferred, not rejected. No one has asked for it, and building it would add a permission dimension nobody uses. Revisit when there is a concrete ask.
- **Hide existence (404 for both unknown and forbidden).** Rejected while group names are observable through their subdomains. It would cost agents an actionable error for no real secrecy.

## See also

- [0011-route-ownership.md](0011-route-ownership.md): the policy whose read half this replaces; owner-write stays
- [0030-virtual-host-routing.md](0030-virtual-host-routing.md): the multi-tenant commitment that removed 0011's premise
- [0039-structural-admin-gating.md](0039-structural-admin-gating.md): admin gating is structural; owner-or-admin stays per-resource
- [0012-api-tokens.md](0012-api-tokens.md): token blast radius was described in terms of others-read and shrinks to the user's own routes under this ADR
- ../virtual-host-impact.md: where owned-only lists and owner-visible unmatched were first decided in practice
- ../rest-api.md, ../auth-and-authz.md, ../mcp-surface.md: design docs this updates
