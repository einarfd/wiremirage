# ADR-0011: Route ownership and authorization policy

**Status:** Accepted

**Context:** With multiple authenticated users sharing a WireMirage instance ([0010-oauth-oidc.md](0010-oauth-oidc.md)), we need a policy for who can do what to whose routes and groups. The policy options form a spectrum:

- **Fully shared** — anyone authenticated can read and write any route. Owner is informational.
- **Owner-write, others-read** — anyone can read; only the owner can edit or delete.
- **Owner-only** — owner can read and write; nothing is shared.

The deployment context is a small team of engineers (single-digit to low-double-digit users typically). The expected workflow includes both individual experimentation and shared mocks the whole team uses for integration tests.

**Decision:** Owner-write, others-read.

- Every route and every group records its `owner` (a user ID, set at creation from the auth context).
- Read access (list, view, query journal) is **available to any authenticated user**.
- Write access (create, update, delete) on a route or group is **restricted to its owner**.
- Creating a route in a group requires owning the group.
- The default UI/API view is "things I own"; toggling to "all" shows everyone's.
- Admins (a configurable list of user IDs with elevated permissions) can write anything. Used sparingly — primarily for cleanup and tenant management.

**Consequences:**

- **Cross-team learning is supported.** Engineers can browse what their colleagues have set up, copy patterns, learn from working examples. Closing this off would lose meaningful value.
- **Accidental clobbering is prevented.** A teammate cannot delete or modify your routes by mistake. Coordination conflicts are avoided.
- **The policy is simple to explain.** "Read everyone's, write your own." Two-line summary.
- **Easy to relax later, hard to tighten later.** Going from owner-write to fully-shared would surprise no one. Going from fully-shared to owner-write would break workflows users had built. Starting strict is the safer bet.
- **Implementation is straightforward.** Every write endpoint checks `route.owner == current_user.id || current_user.is_admin`. Every read endpoint is unrestricted within the authenticated user set.
- **Cost: shared write access requires a shared user.** Two engineers wanting joint write access to the same routes would need a "service account" — a user record whose API token is given to multiple agents. This is acceptable for v1; full team primitives (groups of users with shared write) are a v0.2 question if demand surfaces.
- **Cost: visibility leak.** Any authenticated user can see anyone else's routes, including potentially sensitive details in handler source code. This is the deliberate trade-off — for a team-internal mock service, the value of cross-visibility is real, and "don't put secrets in mock handlers" is a reasonable rule.
- **Cost: no per-route privacy.** A user can't mark a route as "private to me." If they really need privacy, they run a separate WireMirage instance. We don't bring tenant-style isolation into v1.

**Alternatives considered:**

- **Fully shared (anyone read-write everything).** Considered. Rejected because the cost of accidental clobbering outweighs the convenience. Also harder to walk back than "make read access broader."
- **Owner-only (no cross-visibility).** Considered. Rejected because the cross-team learning use case is real and frequently mentioned by users of mock servers in general.
- **Per-route or per-group ACLs (alice can write, bob can read, others nothing).** Real flexibility, real complexity. Rejected for v1; the "either you own it or you don't" model is enough for a small-team tool.
- **Public/private flag per route.** Sometimes-useful but adds a dimension to the model with limited payoff. The "I want this private" use case is better served by a separate instance. Rejected for v1.

**The admin role:**

A small list of user IDs (configured at deploy time as `auth.admins: ["google:alice@acme.example", "github:bob"]`) gets elevated permissions:

- Write access to anyone's routes and groups
- Visibility of all journal entries and sessions
- Ability to revoke other users' API tokens
- Ability to delete user accounts

Admins are operators, not normal users. The role exists for cleanup, fixing accidents, and decommissioning. It's intentionally not exposed in the UI for non-admin users; admins discover the role through documentation.

**Visibility outside authenticated users:**

The public mock-traffic listener (where SUTs send requests) is unauthenticated by design — adding auth would defeat the purpose. Routes are fired based on path/method matching alone, irrespective of who's calling. This is the same model as every other mock server.

The unauthenticated surface is *only* the mock-traffic listener. Everything else (REST API, MCP, UI, login flow) requires authentication. The unmatched-request log is visible only to authenticated users despite being populated by unauthenticated traffic.

See also: [0010-oauth-oidc.md](0010-oauth-oidc.md), ../auth-and-authz.md, [0012-api-tokens.md](0012-api-tokens.md).
