# Architecture Decision Records

This folder holds the ADRs (Architecture Decision Records) for WireMirage. Each one captures a single significant decision: the context that motivated it, the decision itself, the consequences (intended and accepted), and the alternatives that were considered and rejected.

> **How to read these.** An ADR is the reasoning record, not reference documentation: it describes a decision as it was made, and the code is the source of truth for current behaviour. The longer design documents referenced throughout — `route-model.md`, `storage-model.md`, `rest-api.md` and the rest — live in the design workspace alongside these ADRs and are not published with them; the repo covers the same ground for users under `docs/`.

The point of writing these down is that design pressure to revisit decisions comes constantly, and not having the original reasoning makes every revisit start from zero. With ADRs, a future "why did we go with Valkey instead of redb?" has an answer — and importantly, "we revised this decision; here's why" is also captured (see [0005-valkey-storage.md](0005-valkey-storage.md) and [0008-handlers-in-storage.md](0008-handlers-in-storage.md)).

## Decisions

- [0001-rust-host.md](0001-rust-host.md) — Rust for the implementation language
- [0002-wasm-sandbox.md](0002-wasm-sandbox.md) — Wasm via wasmtime for handler execution
- [0003-component-model.md](0003-component-model.md) — Wasm Component Model and WIT for the script API
- [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md) — compiler-as-sidecar architecture
- [0005-valkey-storage.md](0005-valkey-storage.md) — Valkey as the storage backend (supersedes the original redb proposal)
- [0006-recording-separate.md](0006-recording-separate.md) — record-and-replay is a separate tool
- [0007-typescript-first.md](0007-typescript-first.md) — TypeScript as the first scripting language
- [0008-handlers-in-storage.md](0008-handlers-in-storage.md) — routes live in storage, not on disk (supersedes the original on-disk proposal)
- [0009-html-htmx-ui.md](0009-html-htmx-ui.md) — server-rendered web UI (minijinja templates, vanilla JS, Ace editor)
- [0010-oauth-oidc.md](0010-oauth-oidc.md) — OAuth 2.0 / OIDC for authentication
- [0011-route-ownership.md](0011-route-ownership.md) — route ownership and authorization policy
- [0012-api-tokens.md](0012-api-tokens.md) — API tokens for programmatic access
- [0013-groups-first-class.md](0013-groups-first-class.md) — groups as first-class lifecycle units
- [0014-valkey-not-redis.md](0014-valkey-not-redis.md) — Valkey, not Redis (licensing)
- [0015-cli-skill-primary-mcp-secondary.md](0015-cli-skill-primary-mcp-secondary.md) — CLI plus skill as the primary agent surface; MCP as secondary
- [0016-ai-friendly-identifiers.md](0016-ai-friendly-identifiers.md) — AI-friendly identifier scheme (slugs and per-parent numbers, not ULIDs externally)
- [0017-observability-tracing.md](0017-observability-tracing.md) — host observability via `tracing` + opt-in OTLP
- [0018-local-user-accounts.md](0018-local-user-accounts.md) — local user accounts via env var (scoped exception to ADR-0010 for testing and small deployments)
- [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md) — MCP-client OAuth Authorization Server scoped to `/__api/mcp` (Claude Desktop, Inspector, etc.)
- [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md) — vendor one wasm engine per interpreted language, store source on routes (partially supersedes ADR-0004 for interpreted languages)
- [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) — add `sleep` / `wall-time-ms` / `monotonic-ms` to the handler WIT contract
- [0022-streaming-http-responses.md](0022-streaming-http-responses.md) — streaming HTTP responses via a writer resource on a generic byte-stream substrate (SSE, MCP transport; keeps the door open for WebSocket/gRPC)
- [0023-source-only-public-handler-input.md](0023-source-only-public-handler-input.md) — drop pre-compiled-wasm upload from the public surface; source + language is the only public handler input (lands before ADR-0022)
- [0024-metrics-via-otlp.md](0024-metrics-via-otlp.md) — metrics over the existing OTLP/gRPC pipeline (dispatch, handler resources, streaming); extends ADR-0017's deferred-metrics decision
- [0025-writable-handler-state.md](0025-writable-handler-state.md) — add write + snapshot to the external state API (seed/reset handler `kv:`/`gkv:` from outside a handler); reusable-mock *bundle* format deferred
- [0026-string-first-body-encoding.md](0026-string-first-body-encoding.md) — extend ADR-0025's string-first (`string | {base64}`) encoding to request/response bodies (journal, unmatched, dry-run); retire array-of-ints / `body_b64` on the public surface
- [0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md) — collapse `WM_SECURE_COOKIES` / `WM_TRUST_FORWARDED_HEADERS` / `WM_MCP_ALLOWED_HOSTS` into one `WM_TRUSTED_PROXY=<host>` switch; removes the behind-a-proxy partial-config footgun
- [0028-trailing-segment-path-matcher.md](0028-trailing-segment-path-matcher.md) — trailing-segment matcher `{path...}` for catch-all / echo routes (prefix-capable, zero-or-more, lowest precedence). **Implemented** 2026-06-07 as a lowest-precedence backstop; the unmatched journal + `show_unmatched` remain the discovery path
- [0029-group-scoped-namespacing.md](0029-group-scoped-namespacing.md) — revisits the flat namespace after first-user multi-tenant feedback; the A/B/C options analysis (**superseded by ADR-0030**, which chose C)
- [0030-virtual-host-routing.md](0030-virtual-host-routing.md) — per-group subdomains; mock traffic on `{group}.{apex}`, the apex is control-plane only; `(host, method, path)` identity, per-host conflict detection, wildcard DNS + DNS-01 TLS. Supersedes route-model.md's flat namespace
- [0031-reusable-mock-bundles.md](0031-reusable-mock-bundles.md) — a bundle is a single JSON document (routes + state + knob manifest); install via CLI/MCP/REST, no zip/multipart; large binary seed goes via the state API. Multi-instance via ADR-0030 subdomains
- [0032-sandbox-limits-multi-tenant.md](0032-sandbox-limits-multi-tenant.md) — keep the wall-clock bound (fuel can't meter `sleep`/backpressure); make it a per-route budget within a ceiling; per-tenant concurrency deferred. Amends ADR-0002
- [0033-drop-control-plane-prefix.md](0033-drop-control-plane-prefix.md) — drop the `__` control-plane path prefix (`/__api`→`/api`, etc.) and make control-plane routing apex-only, so subdomains can mock `/health`, `/api/*`. Breaking cutover; supersedes route-model.md's reserved-path scheme.
- [0034-outbound-callbacks.md](0034-outbound-callbacks.md) — outbound callbacks/webhooks: host-orchestrated `scheduleCallback` (single-attempt, best-effort, journaled), deployment-gated egress (hardcoded special-use default-deny + `WM_EGRESS_ALLOW` override, resolved-IP enforcement), per-group `callout_enabled` opt-in.
- [0035-generic-oidc-login.md](0035-generic-oidc-login.md) — one generic OIDC login provider (issuer URL + discovery, code flow + PKCE, userinfo-based identity) makes Pocket ID / Keycloak / any compliant IdP configuration-only; hand-rolled on reqwest rather than the `openidconnect` crate ADR-0010 named. Extends ADR-0010's v1 provider narrowing.
- [0036-email-only-identity.md](0036-email-only-identity.md) — the verified email IS the account: unique identifier, display label, and admin-surface selector on every surface; usernames (and the derived-handle scheme) deleted; logins without a verified email refused; local-auth and bootstrap identifiers are emails. Amends user-model.md; cuts across ADR-0010/0018/0035.
- [0037-multi-replica-readiness.md](0037-multi-replica-readiness.md) — the per-process state blocking more than one replica: route-table coherence (read-through floor + pub/sub invalidation), journal live-tail fan-out over Valkey pub/sub, a stateless MCP transport, plus the throttle/sweeper/bootstrap fixes. Corrects storage-model.md's cache-coherence guarantee, which describes a read-through that was never implemented.
- [0038-one-transpiler-tsc-as-checker.md](0038-one-transpiler-tsc-as-checker.md) — swc emits every TypeScript→JavaScript byte, the js-engine build included; `tsc` stays on the maintained major as an `engine.ts`-only checker (nothing type-checked it before), and the handler `.d.ts` ADR-0007 promised finally ships — types where the author is, still no checker in the create path. Amends ADR-0020's build shape without touching its no-tsc-for-handlers argument

## Conventions

ADRs are numbered sequentially. Numbers are never reused, even if a decision is later reversed. Instead, the old ADR is updated in place with a "Supersedes ADR-NNNN v1" header, and the body is rewritten to reflect the current decision. This keeps the file's number stable across the project's lifetime and avoids gaps in the sequence.

For substantive supersessions, the previous decision is briefly summarized at the top of the new content so the historical reasoning isn't lost. The full prior version is recoverable from Arkiv's file version history if needed.

Each ADR has a `Status` field at the top: typically `Accepted` for active decisions, `Superseded by ADR-NNNN` for ones replaced by a different-numbered ADR, `Deprecated` for ones no longer relevant. `Proposed` is used briefly while a decision is under discussion.

The structure is consistent across files: Context, Decision, Consequences, Alternatives Considered, with a "See also" pointer to related ADRs and design docs at the end.
