# ADR-0019: MCP-client OAuth authorization for /__api/mcp

**Status:** Accepted

**Context:**

[0012-api-tokens.md](0012-api-tokens.md) says "bearer tokens for the API and MCP need a separate path. OAuth … isn't appropriate for long-lived programmatic access." That framing was correct for the original audience: CLI agents (Claude Code, `wm` in a CI script, Cursor's Bash tool) that read a token from an environment variable and call REST or MCP non-interactively. For those clients, OAuth would be friction with no payoff.

It is incorrect for native MCP clients. **Claude Desktop, Cursor's MCP integration, the MCP Inspector, and any client built against the streamable-HTTP transport with the modern auth profile** authenticate a human user once and then hold tokens. They speak the MCP-spec OAuth dance and have no UX for "go to the WireMirage web UI, mint a long-lived `wmt_` token, paste it into a JSON config file." Without first-class OAuth support, WireMirage is unusable from those clients.

The MCP specification 2025-06-18 §2 Authorization mandates OAuth 2.1 with PKCE, with discovery via RFC 9728 (Protected Resource Metadata) and RFC 8414 (Authorization Server Metadata), and dynamic client registration via RFC 7591. Native clients implement this; resource servers that want to play expose the matching endpoints.

This is purely an interactive-client problem. The headless / CLI / CI surface keeps using `wmt_` bearer tokens; nothing changes for them.

**Decision:**

WireMirage adds an OAuth 2.1 Authorization Server scoped to its own MCP resource. The full surface:

| Endpoint | Purpose |
|---|---|
| `GET /.well-known/oauth-protected-resource` | RFC 9728 — names the AS for `https://host/__api/mcp` |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 — names authorize/token/register/revoke endpoints, supported grants, PKCE method |
| `POST /__auth/oauth/register` | RFC 7591 — Dynamic Client Registration (public, no auth) |
| `GET /__auth/oauth/authorize` | OAuth 2.1 authorization request; requires UI session |
| `GET /__ui/oauth/consent` | Approve / Deny screen for the resource-owner step |
| `POST /__ui/oauth/consent` | Consent form submission (CSRF-protected) |
| `POST /__auth/oauth/token` | `authorization_code` (+ PKCE verifier) and `refresh_token` grants |
| `POST /__auth/oauth/revoke` | RFC 7009 token revocation |

Tokens issued via this flow are opaque `wmm_<random>` strings stored in Valkey alongside existing `wmt_` tokens. Both look up to the same user record; both honour the same authorization policy. Access tokens are short-lived (1h); refresh tokens are 30-day, hashed at rest, rotated on every exchange. Both kinds appear on `/__ui/me/tokens` so users can see and revoke them.

The OAuth AS's resource-owner step reuses the existing UI session. If a user hits `/__auth/oauth/authorize` without a `wm_session` cookie, the host redirects to `/__auth/login?next=<authorize-url>` and resumes after login. No second identity primitive.

PKCE with `S256` is mandatory. The host rejects `plain` and rejects requests without a code challenge. Redirect URIs are validated per RFC 8252: exact-match against registered URIs *except* loopback addresses (`http://127.0.0.1:<any-port>/path` and `http://[::1]:<any-port>/path`), which match registered loopback URIs ignoring the port. This is non-negotiable — Claude Desktop and Inspector both bind a random loopback port per session.

The MCP endpoint at `/__api/mcp` responds to unauthenticated requests with **401 + `WWW-Authenticate: Bearer resource_metadata="https://host/.well-known/oauth-protected-resource"`** so clients can discover the AS. The rest of `/__api/*` keeps its plain 401 — discovery is MCP-specific.

**Consequences:**

- **Two acceptable bearer-token formats on `/__api/*`.** `wmt_` (manually-minted, long-lived, unchanged) and `wmm_` (OAuth-flow-minted, short-lived + refreshable). The auth extractor's token-classification step grows one branch; everything downstream is the same.
- **Existing bearer-token surface is unchanged.** No protocol changes for CLI, MCP-from-CLI-token, or REST clients.
- **Four new Valkey keyspaces:**
  - `oauth:client:{client_id}` → client registration (name, hashed secret, redirect URIs, auth method, created_at, last_used_at, revoked_at).
  - `oauth:pending:{state}` → bridge between `/authorize` and the consent page (client_id, redirect_uri, code_challenge, scope, original_state, expires_at). 10-minute TTL.
  - `oauth:code:{code}` → issued authorization code awaiting exchange (client_id, user_id, redirect_uri, scope, code_challenge, expires_at, used_at). 10-minute TTL.
  - `oauth:refresh:{token_hash}` → refresh-token record (client_id, user_id, scope, expires_at, used_at, revoked_at). 30-day TTL.
- **DCR is public.** RFC 7591 expects unauthenticated registration; the spec is fine with this. Mitigations: per-IP rate-limit on `/register`, generated `client_secret` only returned once at registration time, `redirect_uris` validated against the loopback / HTTPS rules above. The host caps registered-clients-per-IP to prevent disk-fill abuse.
- **Tokens page grows a "kind" column.** Each row shows `wmt` (manual) or `wmm` (oauth, client name). Revocation works the same for both; OAuth-issued tokens additionally cascade to all refresh tokens for that client.
- **Audit trail.** Each consent decision (approve / deny) writes a journal entry tagged `oauth_consent` so admins can see "Claude Desktop registered at 14:02, user `einar` approved access at 14:03." Out of scope for v1: revoking a client wholesale via the admin UI (CLI/REST suffices initially).
- **More attack surface.** Three new public endpoints (`/.well-known/*` ×2, `/register`) and two new "public-given-a-valid-token-or-code" endpoints (`/token`, `/revoke`). Mitigations are conventional: short TTLs on codes (10 min), rotation on refresh, hashed-at-rest secrets and refresh tokens, PKCE-mandatory, redirect-URI exact-match.
- **Operationally:** `WM_LOCAL_AUTH` works for dev; production deployments still need real user-login ([0010-oauth-oidc.md](0010-oauth-oidc.md) OAuth or local-auth on a trusted network). The AS itself doesn't add a config knob beyond what's needed for token signing — but since we're using opaque tokens stored in Valkey, no JWT secret is required.

**Alternatives considered:**

- **Bearer tokens only; accept that Claude Desktop is unusable.** Rejected. Claude Desktop is the reference native MCP client; first-class support is table stakes for an "MCP-friendly" mock server.
- **Defer to v0.2.** Considered. The MCP auth spec is stable enough to commit to, and shipping without OAuth bakes in a "WireMirage doesn't work with Claude Desktop" reputation that's expensive to undo. Worth doing now.
- **Stateless JWT access tokens** (Arkiv's choice). Tempting for symmetry. Rejected because (a) we already have a Valkey-backed opaque-token model and reusing it removes a second cryptographic surface, (b) instant server-side revocation matters for short-lived tokens issued to forgotten devices, (c) Valkey lookup is sub-millisecond so the "stateless" win doesn't pay rent here. We can revisit if Valkey becomes a bottleneck.
- **Run Authentik / Keycloak as a sidecar AS.** Symmetric with [0010-oauth-oidc.md](0010-oauth-oidc.md)'s rejection of an external IdP for user-login. Single application, no federation requirement, embedded AS is much simpler.
- **Use rmcp's OAuth helpers if they exist.** To verify during slice A. As of writing, rmcp 1.6 ships transport + tool routing but not an AS implementation; we hand-roll axum endpoints. Wire shape is unchanged either way.

**Implementation order:**

Ships after [0010-oauth-oidc.md](0010-oauth-oidc.md) (user-login OAuth) is live in production, because the AS's resource-owner step needs a real user session. Local-auth ([0018-local-user-accounts.md](0018-local-user-accounts.md)) is enough for dev. Suggested slice breakdown:

1. **Slice A — discovery + storage.** Add the four Valkey keyspaces and the two `.well-known/*` endpoints (static metadata, no flow yet). Returns valid JSON; a client running discovery sees a well-formed AS.
2. **Slice B — happy path.** DCR + authorize + consent + token (authorization_code grant). End-to-end against MCP Inspector with PKCE. UI surface: a consent page.
3. **Slice C — refresh + revoke + UI surface.** Refresh-token rotation + revoke endpoint + "kind" column on `/__ui/me/tokens` with per-row revoke.
4. **Slice D — hardening.** DCR rate-limit, `WWW-Authenticate` discovery hint on `/__api/mcp` 401, audit-log of consent decisions, redirect-URI validation polish.

**Reference implementation:**

Arkiv's `src/arkiv/mcp/oauth_provider.py`, `src/arkiv/ui/oauth_consent.py`, `migrations/0001_initial_schema.sql` (oauth_* tables), and `queries/oauth.sql`. They use FastMCP's `OAuthProvider` as scaffolding; we'd implement equivalent flow logic on axum. Their token model is JWT-based (different choice from us, but the rest — DCR, consent page, refresh rotation, code lifetimes — translates directly).

**See also:**
- [0010-oauth-oidc.md](0010-oauth-oidc.md) — user-login OAuth. Different audience (browsers, not MCP clients), shared infrastructure for PKCE and session-cookie issuance.
- [0012-api-tokens.md](0012-api-tokens.md) — the existing `wmt_` bearer-token model that `wmm_` shadows. Needs an "MCP-issued tokens" section once this ships.
- ../auth-and-authz.md — needs an "MCP-client OAuth" section.
- MCP specification 2025-06-18, §2 Authorization.
- RFC 7591 (Dynamic Client Registration), RFC 8414 (AS Metadata), RFC 9728 (Protected Resource Metadata), RFC 7636 (PKCE), RFC 7009 (Token Revocation), RFC 8252 (OAuth 2.0 for Native Apps).
