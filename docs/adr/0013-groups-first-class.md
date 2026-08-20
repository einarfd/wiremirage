# ADR-0013: Groups as first-class lifecycle units

**Status:** Accepted

**Context:** A "group" appeared in early design as a way for related routes to share runtime state — the `group-store` in ../script-api-wit.md. As the design has matured, two more uses emerged:

1. **Lifecycle.** A "Stripe API mock" is conceptually a set of routes (charges, refunds, customers, subscriptions, webhooks) that should be created together, expire together, be inspected together, and be deleted as a unit. Without grouping at this layer, you'd be deleting 12 routes one at a time and hoping you got them all.
2. **TTL ergonomics.** With routes ephemeral by default ([0008-handlers-in-storage.md](0008-handlers-in-storage.md)), where does TTL live? On individual routes? Then deleting a "Stripe mock" requires touching every route's expiry. On groups? Then routes inherit naturally.
3. **Visibility scoping.** Once routes have ownership ([0011-route-ownership.md](0011-route-ownership.md)), the question of "show me everything in the Stripe mock my colleague set up" wants a group-level access pattern, not 12 individual route lookups.

These three uses point at the same underlying concept: a group is a **lifecycle and management unit**, distinct from but containing routes.

**Decision:** Groups are first-class objects in WireMirage's data model, alongside (and containing) routes.

A group has:
- An ID (ULID)
- A name (human-readable, owner-assigned)
- An owner (user ID, derived from auth at creation)
- A TTL (default 24h, max 30d, optional sliding behavior)
- Optional description
- A set of routes that belong to it

Routes belong to exactly one group. A standalone route is implemented as a group containing one route, with `implicit: true` for UI display purposes.

Operations:
- `POST /__api/groups` — create a group, optionally with initial routes
- `GET /__api/groups/{group}` — fetch a group with its routes
- `PATCH /__api/groups/{group}` — update name, description, TTL, sliding flag
- `DELETE /__api/groups/{group}` — delete group and all routes atomically
- `POST /__api/groups/{group}/refresh` — bump expiry (for sliding-TTL groups, automatic; this endpoint is for explicit "I want this to live another 24h" use)
- `POST /__api/routes` — create a route in a group (specify group by name in the body)
- `DELETE /__api/routes/{group}/{n}` — delete a single route

The CLI exposes equivalent commands; the MCP server exposes equivalent tools. See ../rest-api.md for full endpoint details.

**Consequences:**

- **TTL semantics are simple.** TTL lives on the group. Routes inherit. Deletion cascades. One model to reason about.
- **The "Stripe mock" use case is natural.** Create a group "stripe-mock", populate with the relevant routes, hand the group ID to your test or CI script. When done, delete the group; everything goes away.
- **The UI has a clear primary unit.** The web UI ([0009-html-htmx-ui.md](0009-html-htmx-ui.md)) lists groups, with routes nested. "Alice's groups: stripe-mock (12 routes), pubsub-mock (4 routes), my-scratch (1 route)." Clean.
- **Single-route groups need a UI special case.** If `implicit && route_count == 1`, display as the route directly to avoid noise. Single-route groups created explicitly (the user named them) display as groups regardless. Modest UI complexity in exchange for a uniform underlying model.
- **Route paths are unique within a group, not across groups.** Under virtual-host routing ([0030-virtual-host-routing.md](0030-virtual-host-routing.md)) each group is its own subdomain and path space, so a route in group A and a route in group B *may* share `(method, path-pattern)`; the conflict-detection check at creation is scoped per-group. (This originally read "unique across all groups" under the v1 flat namespace of ../route-model.md; ADR-0030 superseded that.)
- **Sliding TTL belongs at the group level.** Sliding-on-route would mean a partially-expired group (some routes alive, some dead), which is hard to reason about. Sliding-on-group keeps the whole API mock alive while in use. v1 supports this; per-route sliding is deferred.
- **Group state (`group-store`) is naturally scoped.** A handler's `group-store` is keyed by `gkv:{group_id}:*` and is automatically cleaned when the group expires. The cascade-delete logic that handles routes also handles group state.

**Alternatives considered:**

- **No groups; tags or labels instead.** Routes have a `tags: list<string>` field; the UI groups by tag. Simpler data model but loses the lifecycle property — you'd still need to delete each route individually, expiry would still need to be per-route, etc. Tags solve a different problem (cross-cutting categorization) than lifecycle units.
- **Implicit groups via path prefix.** Routes under `/stripe/*` are "the Stripe mock," automatically grouped. Tempting because it's free, but conflates the routing space with the management space. Two separate concerns; better as separate concepts.
- **Multiple groups per route.** A route could belong to several groups. Considered briefly. Rejected because the lifecycle question gets ambiguous (whose TTL applies? what does "delete this group" mean if the route is in another group too?). Single-parent-group is a much cleaner model.
- **No grouping at all.** Considered; rejected because the "delete a coherent set of mocks" use case is real and expected to be common.

See also: [0008-handlers-in-storage.md](0008-handlers-in-storage.md), ../route-model.md, ../storage-model.md, [0011-route-ownership.md](0011-route-ownership.md).
