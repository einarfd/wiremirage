# ADR-0030: Virtual-host routing — per-group subdomains (mock traffic on subdomains, apex is control-plane)

**Status:** Accepted

**Supersedes:** the flat-namespace decision in ../route-model.md and its "v0.2 virtual-host" sketch; decides the A/B/C question analyzed in [0029-group-scoped-namespacing.md](0029-group-scoped-namespacing.md).

**Context:**

../route-model.md chose a **flat namespace** for v1 — one global path space, host-wide `(method, path)` conflict detection — with separate deployments as the answer for hard cases and virtual-host routing named as the deferred "v0.2" direction *if demand became concrete*. [0029-group-scoped-namespacing.md](0029-group-scoped-namespacing.md) revisited this after first-user feedback and laid out three options: **A** keep flat, **B** opt-in per-group path prefix, **C** per-group subdomain (virtual host). It initially leaned "keep flat, defer."

That conclusion was overtaken by an explicit statement of intent: **the single deployment (`wm.example.com`) is meant to be genuinely multi-tenant** — more people will use it. The rest of the product already assumes this (user accounts, route ownership, API tokens, admin-vs-owner authorization); the flat namespace is the lone single-tenant remnant, and it is inconsistent with everything around it. Two concrete drivers sharpen the need beyond "convenience":

1. **Multiple isolated instances of a reusable mock bundle on one host.** Deploying the same bundle (Stripe, an OpenAPI surface, the s3-slowdown mock) twice is *impossible* today, not merely tedious: the second copy's routes 409 against host-wide conflict detection, so you must rewrite every route path. A per-tenant root space makes a second copy a one-line operation. (See [0031-reusable-mock-bundles.md](0031-reusable-mock-bundles.md).)
2. **Spec-fixed, root-anchored paths.** `/.well-known/openid-configuration` and friends (OAuth/OIDC discovery, JWKS, `robots.txt`) must live at an exact root path — the client won't look elsewhere. Two tenants cannot both mock the same one under a flat namespace, and convention/prefix can't help because the SUT calls the literal path. This is the one wall nothing else scales past.

Option **B** (path prefix) is cheap and infra-free but can't make cohabitation automatic and *cannot* host two copies of a root-anchored spec-fixed path (an SDK won't prepend a prefix to `/.well-known/…`). Option **C** solves both and preserves full path fidelity. The operator has accepted the cost (wildcard DNS + TLS) and a one-time breaking cutover, because the deployment is ephemeral-by-design with no backups (see the project's no-backup posture) — wiping and re-registering is free.

**Decision:**

Adopt **virtual-host routing**: a route's match identity becomes **`(host, method, path)`**, and each group is bound to a subdomain `{group}.{apex}` (e.g. `stripe-mock.wm.example.com`).

- **Mock traffic is served ONLY on group subdomains.** The apex (`wm.example.com`) is **control-plane only**: `/__ui`, `/__api` (incl. the `/__api/mcp` MCP endpoint), `/__auth`, `/__health`, `/__ready`. The apex serves **no** user mock traffic. This is a clean break — no host-less/shared apex mock space — accepted because the single deployment's data is ephemeral and wiped on cutover.
- **Dispatch resolves Host → group, then matches `(method, path)` within that group's routes only.** Conflict detection scopes per host: two groups may both define `/v1/charges`. The existing segment-compatibility rules apply *within* a host.
- **Group name = the DNS label.** Lowercase `[a-z0-9-]`, ≤63 chars, no leading/trailing hyphen; already globally unique (the `group:by-name` index enforces it). Validated at create + rename.
- **Group name becomes optional → the server auto-assigns an AI-friendly name.** Creating a group (or a route with no `--group`) without a name yields a generated **adjective-noun** label, with a numeric disambiguator on collision — `swift-otter`, `calm-harbor`, `amber-finch-3`. This satisfies all three constraints at once: DNS-safe by construction, memorable/pronounceable (the AI-friendly property of [0016-ai-friendly-identifiers.md](0016-ai-friendly-identifiers.md), far better than a ULID in a subdomain), and token-efficient (~2–3 tokens vs a ULID's ~8–10). Source the words from a sufficiently large list — a lightweight maintained crate (`names` is essentially just `rand`, which we already depend on; `petname` / `memorable-wordlist` are alternatives) or a curated static list (zero deps, full control over DNS-safety / word choice) — and **normalize the output to a DNS label** regardless. 65k two-word combos is on the small side, so favour a larger list and/or the numeric disambiguator. This **replaces** the old `_route_{ulid}` implicit-group naming (illegal leading underscore anyway) and **unifies** it with explicit name-less creation — implicit single-route groups and "I didn't pick a name" groups get the same friendly scheme. Raw ULIDs stay internal only, per [0016-ai-friendly-identifiers.md](0016-ai-friendly-identifiers.md).
- **Each subdomain is a full root path space**, so spec-fixed paths coexist across tenants: `alice-oidc.{apex}/.well-known/openid-configuration` and `bob-oidc.{apex}/.well-known/openid-configuration` simultaneously. A subdomain `GET /` serves that group's root route or 404s; the **apex** `GET /` keeps the UI/login redirect. A request to an **unknown subdomain** (no matching group) returns a clean 404 and is **not** written to the unmatched journal (no group to attribute it to).
- **Unmatched, near-miss, match-probe, journal, and the bare-`/` redirect all become host-scoped** → per-tenant visibility (an improvement over today's single admin-only global unmatched pile).
- **Surfaces report the group's URL**: the Connect page, `summarize_workspace`/`who_am_i` `base_url`, `show_route`, and `wm` output report the group's subdomain, not the apex. (The `public_base_url` work from ADR-0027 becomes per-group-aware; auth/OAuth redirect URIs stay apex-derived.)
- **Deployment**: wildcard DNS `*.{apex}` → the box; **wildcard TLS via DNS-01** (HTTP-01 cannot issue wildcards — a new ops dependency: a DNS-provider API token); proxy wildcard vhost forwarding the original `Host`; `WM_TRUSTED_PROXY` ([0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md)) extended to accept the wildcard.
- **Metrics**: host/group is **not** a metric label (preserve the bounded-cardinality decision in [0024-metrics-via-otlp.md](0024-metrics-via-otlp.md)); it may be a span attribute.
- **Auth/cookies/CSRF stay apex-only** (mock subdomains are unauthenticated by design), so there is no cookie-domain complexity.

**Phased plan** (each phase reviewable; do not start before this ADR is accepted):

1. **Data model + matching (app-only, in-memory testable):** add the host/group binding; per-host `find_match` + per-host conflict detection; group-name DNS-label validation; implicit-group rename; apex stops serving mock traffic.
2. **Deployment:** wildcard DNS + DNS-01 wildcard cert + proxy wildcard vhost + `WM_TRUSTED_PROXY` wildcard. One-time data wipe at cutover.
3. **Surfaces + follow-ups:** per-group URLs across UI/CLI/MCP/Connect; host-scoped journal/unmatched/probe. (This phase originally also planned to **re-enable the deferred catch-all** — [0028-trailing-segment-path-matcher.md](0028-trailing-segment-path-matcher.md) — on the reasoning that a `{path...}` scoped to one subdomain is a bounded per-tenant backstop rather than a host-wide takeover. That was reconsidered post-acceptance: the catch-all was **kept Deferred** (2026-06-03), since the discovery need it targeted is already served by the unmatched journal + `show_unmatched`.)

**Consequences:**

- **Real multi-tenancy.** Tenants get isolated subdomains with a full root path space and no collisions; the ownership/token/authz machinery finally has a routing model that matches it.
- **Spec-fixed-path cohabitation is solved** (OAuth/OIDC discovery, JWKS, `robots.txt`) — the one genuine wall.
- **Reusable bundles become multi-instance** — same bundle → N groups → N isolated subdomains ([0031-reusable-mock-bundles.md](0031-reusable-mock-bundles.md)).
- **The catch-all became bounded per subdomain**, which removed the host-wide-takeover objection. (Post-acceptance it was nonetheless **kept Deferred** — [0028-trailing-segment-path-matcher.md](0028-trailing-segment-path-matcher.md), 2026-06-03 — as the discovery need was already met by the unmatched journal + `show_unmatched`.)
- **Cost: a cross-cutting epic** (multi-week), spanning `wm-host` and the deployment repo. New ops dependency (DNS-01 wildcard cert). Group **rename changes a tenant's URL**; it's allowed and **returns the new URL** (like create) so the caller repoints their SUT — cheap, since routes/state/journal are ULID-keyed (only the `group:by-name` lookup moves). This *adds* rename, which `update_group` doesn't currently support.
- **One-time breaking cutover**: every existing SUT must repoint to a subdomain; current data is wiped (acceptable — ephemeral by design).
- **Group names lose free-form flexibility** (DNS-label only) and implicit-group names change shape.

**Alternatives considered:**

- **Keep the flat namespace (ADR-0029 option A).** Rejected: inconsistent with the now-explicit multi-tenant goal; cannot solve spec-fixed paths or multi-instance bundles on one host.
- **Per-group path prefix (ADR-0029 option B).** Rejected as the model: infra-free, but cohabitation isn't automatic and it *cannot* host two copies of a root-anchored spec-fixed path. Viable only as a no-infra interim; we choose the clean endpoint directly rather than carry two namespacing mechanisms.
- **Hybrid (apex still serves host-less routes + subdomains add isolation).** Rejected: backward-compat we don't need (ephemeral data, single deploy), and the dual namespace complicates conflict detection for no benefit here.
- **Separate deployment per tenant (route-model.md's v1 answer).** Rejected as the multi-tenant answer: heavier for the operator than one shared host with subdomains, and defeats the shared-fixture goal — though it remains valid for someone who wants hard isolation.

**See also:** ../virtual-host-impact.md (the per-subsystem impact inventory + Phase-1 checklist for this ADR), ../route-model.md, [0029-group-scoped-namespacing.md](0029-group-scoped-namespacing.md), [0013-groups-first-class.md](0013-groups-first-class.md), [0028-trailing-segment-path-matcher.md](0028-trailing-segment-path-matcher.md), [0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md), [0032-sandbox-limits-multi-tenant.md](0032-sandbox-limits-multi-tenant.md), [0031-reusable-mock-bundles.md](0031-reusable-mock-bundles.md), [0016-ai-friendly-identifiers.md](0016-ai-friendly-identifiers.md)
