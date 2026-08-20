# ADR-0029: Group-scoped namespacing — revisiting the flat namespace after first-user multi-tenant feedback

**Status:** Superseded by [0030-virtual-host-routing.md](0030-virtual-host-routing.md)

**Outcome:** This ADR's recommendation was "keep the flat namespace for now." It was overtaken in the same discussion: the operator confirmed the shared host is *intended* to be multi-tenant, which made cohabitation a goal rather than a hypothetical and moved the decision from "defer" to "act." Of the three options analyzed below, **C (per-group subdomain / virtual-host)** was chosen — see [0030-virtual-host-routing.md](0030-virtual-host-routing.md) for the decision and why C beat B (B can't host two copies of a root-anchored spec-fixed path, and cohabitation isn't automatic). This ADR is retained as the A/B/C options analysis that led there.

**Context:**

The first external user driving WireMirage through MCP, against real SDK clients, reported the **flat path namespace** as their top friction: *"All groups share one path space at `wm.example.com/<path>`. Two people mocking Vertex collide, and there's no isolation — I had to smuggle a unique fake region into the URL to namespace my routes. A per-group host or path prefix would make groups real tenancy boundaries. Right now 'group' is only a lifecycle/TTL unit, not a routing one."*

This is a **revisit**, not a fresh decision. ../route-model.md already chose the flat namespace deliberately (familiar WireMock-style model; the SUT gets one base URL; per-subdomain routing means DNS + TLS wildcards we didn't want to take on), already names the cost (team members stepping on each other's paths), and already documents the answers: convention-based prefixes (`/team-foo/…`, `/u/{user}/…`) + strict conflict detection for the common case, **separate deployments** for the hard case, and **virtual-host routing** (`(host, methods, path)` identity, Apache-vhost style) as the explicitly-deferred v0.2 direction *if demand becomes concrete*. The question this ADR answers: does the first-user feedback make that demand concrete enough to act, and if so, how?

Two things have to be separated, because the feedback conflates them:

1. **Arbitrary-path collisions** (the user's actual case: two people mocking Vertex at `/v1/…`). These are **prefix-able** — real APIs are hosted at *some* base URL and the SUT is normally configurable to call whichever prefix the mock lives at. The user *already solved their own case* by namespacing the path (the "fake region" trick). It's friction, not a wall.
2. **Spec-fixed-path collisions** (`/.well-known/openid-configuration`, `/robots.txt`, JWKS, ACME — root-anchored paths the client will not let you move). These are the **genuine wall**: convention can't help, because the SUT calls the exact path or nothing works. This is the only case where the flat namespace truly blocks two users on one instance — and it is *not* what the first user hit.

The new fact that legitimately changes the calculus: the user is running a **single shared host** (`wm.example.com`) with agent access and is explicitly thinking about a "shared CI fixture." The flat namespace was designed assuming the hard cases would be handled by *separate* deployments; a deliberately-shared multi-tenant instance is the shape route-model.md said would, if it recurred, trigger the v0.2 work.

**Decision:**

**Keep the flat namespace for now (no change).** The single concrete data point is a prefix-able collision the user already worked around; the one case that flat namespacing genuinely can't serve (spec-fixed paths on a shared instance) has not been hit. The product's value is *per-deployment fidelity*, and the v1 escape hatch — run a separate WireMirage per developer / per CI job (`docker compose up` is one command) — remains cheap and fully covers the hard case. Building tenancy machinery now would be acting on one prefix-able report; that doesn't clear the bar.

But sharpen the trigger and pre-commit the mechanism, because the user's shared-host usage means this may become real:

- **Trigger to revisit:** a *recurring* need for **multiple users to mock the same spec-fixed (root-anchored) path on one shared instance** — not arbitrary-path collisions, which stay prefix-able. Convenience-level "I wish groups namespaced my arbitrary routes" alone is not the trigger; the workaround (prefix, or a separate instance) exists.
- **When triggered, the recommended mechanism is the cheaper of the two that fits:**
  - **Option B — opt-in per-group path prefix (preferred first step).** A group may declare a `prefix` (e.g. `/alice`); its routes are served under it, and **the prefix is stripped before route matching** so route patterns and the handler's view of the path stay the real API paths (`/v1/charges`, not `/alice/v1/charges`). The SUT sets `base_url = https://host/alice`. **Zero infra** (no DNS/TLS), and it preserves fidelity for the large majority of SDKs that take a base URL *with a path*. It makes "group = a routing/tenancy boundary" real, which is literally what the user asked for. It does **not** solve root-anchored spec-fixed paths (the SDK won't prepend the prefix to `/.well-known/…`).
  - **Option C — virtual-host routing** (route-model.md's v0.2: identity becomes `(host, methods, path)`; `alice.wm.example`/`bob.wm.example`). The only thing that solves spec-fixed-path collisions, and preserves full path fidelity. Reserve it for when B's prefix-stripping isn't enough — i.e. the spec-fixed-path-on-shared-instance case actually recurs — because it carries real weight: wildcard DNS + wildcard TLS on the deployment box, plus a `host` dimension through the data model, per-host conflict detection, and CLI/UI/MCP host-config surfaces.

**Explicitly rejected: the feedback's literal "per-group path prefix" as a naive, always-on, non-stripped rewrite.** Serving routes at `/group/v1/charges` and matching that literally would force handlers and route patterns to carry the tenancy prefix, breaking the path fidelity that is WireMirage's entire reason to exist (conformance against real client libraries). If we do B, the prefix must be stripped before matching so the mocked API keeps its real paths.

**Consequences:**

- **No code or infra change now.** route-model.md's existing flat-namespace + separate-deployments guidance stands; this ADR records that the first-user feedback was weighed and didn't move the decision, and converts route-model.md's vague "if demand becomes concrete" into a concrete trigger (spec-fixed paths on a shared instance) and a staged mechanism (B before C).
- **The user's shared-host friction persists but has workarounds** — path-prefix-by-convention for arbitrary APIs (which they're already doing), or a per-tenant instance for anything that must own a fixed path.
- **The recommendation is now eyes-open about cost.** If the user decides shared multi-tenancy on one box *is* a goal, B is a bounded, no-infra slice they can greenlight; C is a larger, infra-touching slice with deployment-repo implications (wildcard DNS/TLS) and is reserved for the spec-fixed-path case.
- **Forward-compatible, as route-model.md already noted:** adding a `host` dimension (C) or a group `prefix` (B) later doesn't break the v1 `(method, path)` contract — prefix-less / host-less routes are the v1 "any" behavior.
- **This pairs with the deferred [0028-trailing-segment-path-matcher.md](0028-trailing-segment-path-matcher.md):** a per-group prefix (B) is where a group-scoped catch-all (`{path...}` under one tenant's prefix) would stop being "excessive" — the whole-surface backstop becomes bounded to a tenant. If B ever ships, revisit 0028 in that light.

**Alternatives considered:**

- **Build per-group path-prefix (B) now.** Rejected *as a now-action*: it's the right mechanism *if* multi-tenancy is a goal, but one prefix-able data point with an existing workaround doesn't justify even a no-infra slice, and the user themselves flagged it "might not be a good fit." Pre-committed as the recommended first step instead.
- **Build virtual-host routing (C) now.** Rejected: heaviest option (infra + data model), and it solves a problem (spec-fixed-path collisions on a shared instance) nobody has actually hit yet. This is exactly the speculative-build the project avoids.
- **Formalize the convention without routing changes** (e.g. a group "suggested prefix" the UI auto-fills, owner-name nudges). Rejected: adds surface for marginal value; the convention already works socially, and it still doesn't touch the only real wall (spec-fixed paths).
- **Declare the shared-host shape unsupported and push per-deployment isolation harder.** Tempting and partly correct (separate instances *are* the clean answer for the hard case), but too strong — the user is deliberately running a shared host and B is a cheap way to make that pleasant if they want it, so foreclosing it would be premature.

**See also:** ../route-model.md (the flat-namespace decision and its v0.2 virtual-host plan this ADR revisits), [0013-groups-first-class.md](0013-groups-first-class.md) (groups as lifecycle units — B would extend them to routing units), [0028-trailing-segment-path-matcher.md](0028-trailing-segment-path-matcher.md) (deferred catch-all, which a per-group prefix would re-contextualize), [0016-ai-friendly-identifiers.md](0016-ai-friendly-identifiers.md)
