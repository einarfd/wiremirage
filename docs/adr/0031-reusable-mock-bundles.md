# ADR-0031: Reusable mock bundles — a single JSON document

**Status:** Accepted

**Context:**

[0025-writable-handler-state.md](0025-writable-handler-state.md) added writable handler state (seed/reset `kv:`/`gkv:` from outside a handler) and explicitly **deferred** the reusable-mock *bundle* format — "routes + initial state + a knob manifest, packaged as a unit." The virtual-host direction ([0030-virtual-host-routing.md](0030-virtual-host-routing.md)) makes that format concrete and valuable: a packaged mock (Stripe, an OpenAPI-derived surface, the `s3-slowdown` reusable mock) can be deployed as a unit and **stamped out as multiple isolated instances** — one per group, hence one per subdomain.

Today there is no bundle: you assemble a mock route-by-route (`create_route` × N, then `set_route_state`/`set_group_state` to seed). A first AI-agent user was doing exactly this ad-hoc to build scenario mocks for Vertex failover testing. A bundle makes that **atomic, repeatable, and shareable**.

The open questions when this was raised: how is a bundle represented and uploaded — especially over MCP (a zip? multipart parts?) — and does a JSON representation hit size limits on big mocks?

**Decision:**

**A bundle is a single JSON document — no archive, no multipart.** Shape (illustrative):

```json
{
  "metadata": { "name": "stripe-mock", "description": "...", "version": "1.2.0" },
  "routes": [
    { "methods": ["POST"], "path": "/v1/charges", "language": "typescript", "source": "..." }
  ],
  "state": {
    "kv":  { "stripe-mock/1": { "balance": "1000" } },
    "gkv": { "scenario": "happy-path" }
  },
  "knobs": [
    { "key": "scenario", "scope": "gkv", "description": "happy-path | slowdown | outage", "default": "happy-path" }
  ]
}
```

Everything in a bundle is **JSON-native**, so it never needs to be an archive:

- **Routes carry `source` as text** — [0023-source-only-public-handler-input.md](0023-source-only-public-handler-input.md) made source + language the only public handler input; there is no binary wasm to package.
- **State values are `WireBytes`** (`string | { "base64": "..." }`; [0026-string-first-body-encoding.md](0026-string-first-body-encoding.md)) — binary seed rides as base64 text.
- **Knob manifest + metadata** are plain JSON. The knob manifest is the "which levers can I turn, and how" description — the skill-for-a-group idea from ADR-0025's context.

**Install from the same document across all three surfaces, with no upload machinery:**

- **REST**: `POST /__api/bundles` with `{ bundle, group, ... }` — creates the group (and, under [0030-virtual-host-routing.md](0030-virtual-host-routing.md), its subdomain), all routes, and seeds state.
- **MCP**: `install_bundle({ bundle, group })` — the agent passes the JSON object directly as a tool argument. This is the same thing the agent was doing route-by-route, made atomic. No zip, no parts.
- **CLI**: `wm bundle install ./dir --group X` — the CLI packs an **authoring-friendly directory** (a manifest + `handlers/*.ts` + `seed/`) into the wire JSON, so authors keep real `.ts` files in git. `wm bundle export <group>` produces the JSON back (round-trip / sharing), building on the existing `GET .../state?format=snapshot` dump.

**Size — kept simple deliberately.** A bundle fits the 16 MiB `/__api/*` JSON limit (raised in slice 45). Source is text; even a large API surface is well under. The only realistic over-limit case is **seed state with large binary fixtures** (base64 inflates ~33%). The answer is a **convention, not new machinery**:

> A bundle inlines routes + the knob manifest + *small* config state. Large binary fixtures are loaded separately via the existing `PUT .../state` API ([0025-writable-handler-state.md](0025-writable-handler-state.md)) after install.

This keeps the format a single JSON document, keeps bundles small by construction, and forecloses nothing — if a real bundle ever hits the wall, "split the big state out" is the move long before multipart would be. Building chunked/zip upload now would be speculative machinery for a problem the state API already relieves.

**Multi-instance** is the payoff with [0030-virtual-host-routing.md](0030-virtual-host-routing.md): install the same bundle into N groups → N isolated subdomains, each with its own seeded state. The bundle is the template; the group is the instance.

**Consequences:**

- Reusable mocks become first-class and **multi-instance**; directly serves the "shared CI fixture" and "parameterized mock" goals.
- **Uniform across REST / MCP / CLI with zero upload machinery** — the source-only ([0023-source-only-public-handler-input.md](0023-source-only-public-handler-input.md)) and `WireBytes` ([0026-string-first-body-encoding.md](0026-string-first-body-encoding.md)) decisions already made bundles plain JSON.
- CLI keeps git-friendly authoring; agents construct inline; `export` enables round-trip and sharing.
- Bounded by the 16 MiB API limit; large binary seed uses the state API — no new size machinery (consistent with the project's "don't ship speculative defense" posture).
- **Atomicity needs definition** (follow-up): is install all-or-nothing (rollback group + routes + state on any failure) or best-effort? Lean all-or-nothing — a half-installed bundle is worse than a clean failure.

**Alternatives considered:**

- **Zip / archive bundle.** Rejected: nothing in a bundle is binary (source is text, state is `WireBytes`), so an archive adds packaging/unpacking + content-type handling for a payload JSON already carries.
- **Multipart / chunked upload.** Rejected as premature: routes + config fit 16 MiB comfortably; the only over-limit case (large binary seed) has an existing escape hatch (`PUT .../state`).
- **External references** (bundle points at URLs to fetch source/state). Rejected: breaks self-containment and adds fetch + security surface.
- **Always inline per-key state, including large binary.** Rejected for large fixtures (size); fine for small config, which is the inline case.

**See also:** [0025-writable-handler-state.md](0025-writable-handler-state.md) (deferred this format; the inline-small / state-API-large convention), [0030-virtual-host-routing.md](0030-virtual-host-routing.md) (makes bundles multi-instance), [0023-source-only-public-handler-input.md](0023-source-only-public-handler-input.md), [0026-string-first-body-encoding.md](0026-string-first-body-encoding.md), [0013-groups-first-class.md](0013-groups-first-class.md)
