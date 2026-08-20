# ADR-0012: API tokens for agent and programmatic access

**Status:** Accepted

**Context:** Authentication via OAuth ([0010-oauth-oidc.md](0010-oauth-oidc.md)) gives logged-in users a session cookie that authenticates UI requests. That cookie is appropriate for a browser session — short-lived, refreshed on activity, scoped to the device that logged in.

It's the wrong shape for programmatic access:

- Claude Code, Cursor, and similar agents call WireMirage's REST API (via the `wm` CLI in their Bash tool) or its MCP server non-interactively. They can't sit through an OAuth flow on every invocation.
- CI scripts and test harnesses need stable credentials they can put in environment variables.
- The credential's lifetime should be controlled by the user, not by browser session expiry.

We need a separate credential type for programmatic clients that resolves to the same user identity as the browser session.

**Decision:** API tokens, generated and managed by users via the UI, sent as `Authorization: Bearer <token>` headers.

A token has:

- A token string (opaque, ~32 bytes of entropy, prefix-tagged for UI display, e.g. `wmt_a1b2c3d4...`)
- An owner (the user who created it)
- A name (user-supplied, for telling tokens apart in the UI)
- Optional expiry (default unlimited, configurable; may be revoked manually at any time)
- Created-at and last-used-at timestamps
- Optional scopes (deferred to v0.2 — see below)

A request bearing a valid token authenticates as the token's owner, with that user's ownership and authorization policy applied.

**Consequences:**

- **One identity, multiple programmatic clients.** The UI uses session cookies. The REST API, the `wm` CLI (which calls the REST API), and the MCP server all use the same bearer tokens. Every path resolves to the same user record. Routes the agent creates via the CLI are owned by the user, visible in the UI alongside routes the user created in the UI, accessible via the MCP server with the same token.
- **Tokens are user-managed.** Users generate tokens in the UI, copy them to `WM_TOKEN` env var or `~/.config/wiremirage/config.toml` for the CLI, paste them into agent configs, store them in CI secret stores. Revoke when no longer needed. The host has no opinion on what a token is used for.
- **Multiple tokens per user are normal.** Different agents, different machines, different CI environments — each gets its own token. Easier to revoke a single compromised credential than to rotate everything.
- **Token compromise is bounded by user permissions.** A leaked token is exactly as dangerous as the user being compromised. With owner-write-others-read ([0011-route-ownership.md](0011-route-ownership.md)), the blast radius is the user's own routes plus the read-all view of everyone else's. Bad but not catastrophic for a mock service.
- **Last-used-at helps detect inactive tokens.** The UI shows when each token was last used; users can revoke ones they don't recognize or haven't used in months.
- **Cost: token handling is now part of the UX.** Users have to understand "generate a token, paste it into your agent's config, never commit it to Git, revoke it if you suspect compromise." Standard programmer hygiene but worth documenting clearly.
- **Cost: token storage needs care.** Tokens are stored in Valkey hashed (SHA-256) so a database leak doesn't reveal them. The plaintext token is shown to the user only at creation time; we never display it again.
- **Cost: no per-token scoping in v1.** A token has all the permissions of its owner. Users wanting "a token that can create routes but not delete them" or "a token scoped to one group" don't get that in v1. See "Scopes" below.

**Token shape:**

```
wmt_<base62-encoded random>
```

The `wmt_` prefix lets us cheaply detect "this looks like a WireMirage token" in error messages and logging without revealing the full token. Length: 4-character prefix + 24 characters of random = 28 characters total. The random portion provides ~143 bits of entropy.

Tokens are stored in Valkey under `token:{sha256(token)}` as a hash containing owner, name, created-at, expiry, last-used-at. Lookup on every authenticated request is one Valkey hit; we accept the cost (sub-millisecond on loopback).

**Scopes (deferred to v0.2):**

In v1, every token has the full permissions of its owner. v0.2 may introduce scopes such as:

- `groups:read` / `groups:write`
- `routes:read` / `routes:write`
- Resource-specific scopes (`group:01HK...:write`)

The token model is shaped to allow this — there's a `scopes: list<string>` field reserved on the token record from v1, even though it's always `["*"]` in v1.

**API key bootstrapping:**

A new user has zero tokens at first login. They generate one through the UI, copy it, configure their agent, start using it. The flow is documented in `auth-and-authz.md`.

For deployments where the first admin user can't reach the UI (e.g., a headless install), a one-time bootstrap token can be generated via the host CLI (`wiremirage admin issue-token --user <id>`). This is intended for initial setup only; subsequent token creation happens through the UI.

**Alternatives considered:**

- **OAuth-style refresh-token flow for agents.** Agents would receive an access token that expires, refresh it as needed. Standard for SaaS APIs. Rejected: more complexity than agent workflows need, and agent libraries don't all handle refresh-token flows gracefully.
- **mTLS client certificates.** Cryptographically strong, but issuing and rotating certificates is a notable operational burden, and the agent-side libraries vary in mTLS support quality. Rejected for v1.
- **Single shared bearer token for the whole deployment.** What WireMirage looks like with no auth. Rejected because it loses the per-user attribution we want for ownership.
- **Tokens with full user-impersonation OAuth dance.** A token that, when presented, lets us mint short-lived OAuth credentials for that user against the IdP. Possible but operationally complex; rejected as overkill.

See also: [0010-oauth-oidc.md](0010-oauth-oidc.md), [0011-route-ownership.md](0011-route-ownership.md), ../auth-and-authz.md.
