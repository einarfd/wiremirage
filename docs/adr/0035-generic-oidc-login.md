# ADR-0035: Generic OIDC login provider

**Status:** Accepted

**Context:** [0010-oauth-oidc.md](0010-oauth-oidc.md) chose delegated authentication (OAuth 2.0 / OIDC) and narrowed the v1 provider list to GitHub, explicitly keeping the multi-provider machinery in the design "ready to land when a second provider has a concrete user." That user now exists: self-hosted deployments want browser login against their own IdP — the concrete request is **Pocket ID** (a lightweight self-hosted, passkey-first OIDC provider), but the same ask covers Keycloak, Authentik, Zitadel, and the rest of the self-hosted IdP landscape.

GitHub needed a hand-written adapter (`github_oauth.rs`) because GitHub is *not* an OIDC provider — it's plain OAuth 2.0 plus three proprietary REST endpoints we call ourselves to assemble an identity. The question this ADR answers is whether every additional IdP costs another adapter.

It doesn't. OIDC standardizes exactly the parts GitHub made us hand-roll:

- **Endpoint discovery** — every compliant provider serves `{issuer}/.well-known/openid-configuration` listing its authorization, token, userinfo, and JWKS endpoints. The operator configures one issuer URL; no per-provider endpoint knowledge in code.
- **The flow** — authorization-code + PKCE is byte-for-byte identical across providers (RFC 6749 + OIDC Core), including the token-endpoint request/response shape.
- **Identity claims** — the claim names are specified (`sub`, `email`, `email_verified`, `preferred_username`, `name`) and served by a standard userinfo endpoint. No per-provider payload structs.

One generic relying-party implementation therefore covers Pocket ID, Keycloak, Authentik, Authelia, Zitadel, Dex, Kanidm, Okta, Auth0, Google, Microsoft Entra — effectively every modern IdP. The residual per-provider variance (groups claim name, token-endpoint client-auth method, optional claims) is absorbable as configuration, not code.

**Decision:** Add **one generic OIDC provider module** (`oidc.rs` in wm-host), structurally patterned on `github_oauth.rs`, configured entirely by env vars and driven by OIDC discovery. No per-IdP code, ever — a provider that needs custom code is by definition not OIDC-compliant and would fall under ADR-0010's "non-OIDC adapter on demand" rule instead.

Configuration (fail-fast on partial config, mirroring the GitHub rules):

- `WM_OIDC_ISSUER` — the issuer URL. Discovery is fetched at startup; unreachable or malformed discovery refuses startup (no silent fallbacks).
- `WM_OIDC_CLIENT_ID` / `WM_OIDC_CLIENT_SECRET` — both or neither.
- `WM_OIDC_DISPLAY_NAME` — login-button label (e.g. "Pocket ID"); defaults to the issuer host.
- An allow **posture**, exactly one required (refuse-to-start when none is set, same rule as GitHub — and when both are set, since that combination usually means the operator believes the per-identity rules still restrict something):
  - `WM_OIDC_ALLOW_ALL=true` — every user the issuer authenticates is allowed. The right posture for a **private IdP** (Pocket ID, closed-registration Keycloak, corporate Okta): the operator owns the user table, so account existence already *is* the authorization decision, and per-app restrictions belong in the IdP (e.g. Pocket ID's per-client allowed groups). This is the material difference from the GitHub adapter, where the issuer authenticates the whole internet and an RP-side list is mandatory. Explicit opt-in rather than "no rules = everyone" so a generic-OIDC config pointed at a *public* issuer (Google) without rules still refuses startup.
  - Or per-identity rules, OR'd: `WM_OIDC_ALLOW_EMAILS` (exact emails), `WM_OIDC_ALLOW_DOMAINS` (email domains), `WM_OIDC_ALLOW_GROUPS` (values of the groups claim) — for public issuers, or when the operator prefers holding the list on the WireMirage side.
- Admin promotion on login: `WM_OIDC_ADMIN_EMAILS` and/or `WM_OIDC_ADMIN_GROUPS` (optional, like `WM_GITHUB_ADMIN_USERS`).
- `WM_OIDC_GROUPS_CLAIM` — claim name carrying group membership (default `groups`; the one genuinely non-standard corner of OIDC).
- `WM_OIDC_EXTRA_SCOPES` — appended to the base `openid profile email` (some IdPs gate the groups claim behind an extra scope).

Flow and mechanics:

- Authorization-code flow **with PKCE (S256)** plus the same server-side state-nonce round-trip the GitHub flow uses. New endpoints `GET /auth/start/oidc` and `GET /auth/callback/oidc` (GitHub keeps `/auth/callback`); the redirect URI is derived by the existing `public_base_url` / `WM_TRUSTED_PROXY` logic ([0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md)).
- Both token-endpoint client-auth methods (`client_secret_basic`, `client_secret_post`) supported, selected from the discovery document.
- **Identity comes from the userinfo endpoint**, not from validating the ID-token JWT. For a confidential client doing the code flow over TLS directly to the issuer, the userinfo response carries the same trust as the token-endpoint response itself; skipping JWT validation avoids JWKS fetching/caching/rotation and a JWT dependency entirely. The module stays on reqwest + serde like the GitHub one.
- An email claim explicitly marked `email_verified: false` is treated as absent for email/domain allow-rule purposes.
- Provisioning goes through the existing `upsert_oauth_user` with provider `"oidc"` and the `sub` claim as the stable subject. Username is `preferred_username`, falling back to the email local part; name collisions error the same way GitHub's do.
- One OIDC issuer per deployment at v1. GitHub, local auth ([0018-local-user-accounts.md](0018-local-user-accounts.md)), and OIDC are independent and can coexist, same as GitHub + local today.

**Consequences:**

- Pocket ID — and any compliant IdP — becomes configuration-only, exactly as ADR-0010 projected. GitHub remains the only hand-rolled adapter, and future non-OIDC OAuth providers (Bitbucket, etc.) remain "small adapter on demand."
- The groups claim is the acknowledged non-standard bit. The configurable claim name plus email/domain rules as the always-works baseline absorb it; providers with no groups support (e.g. Google) simply never match a group rule.
- The login page can now offer up to three methods (local password, GitHub, OIDC), each independently enabled.
- Tests follow the `github_oauth_e2e.rs` pattern: an endpoints-override struct pointing at an in-process mock issuer (discovery + token + userinfo). A conformance-style lane booting a *real* IdP container (Pocket ID or Dex, both single-container) is a natural follow-up but not part of this slice.
- Cost: one extra HTTP round-trip (userinfo) per login — irrelevant at login frequency.
- **Some IdPs never assert `email_verified`, and that is now fatal** (found dogfooding Authentik, 2026-07). Authentik's stock `email` scope mapping returns a hardcoded `email_verified: False` — it has no mailbox-verification flow of its own to point at — so under [0036-email-only-identity.md](0036-email-only-identity.md), where the verified email IS the account key, a stock Authentik client fails *every* login. Authentik is the confirmed case, not the only expected one: the failure has a second, per-user shape in IdPs that *do* carry a verified flag but default it to false for accounts an admin created directly rather than through an email-confirmation flow (Keycloak's per-user *Email verified* toggle). The generalization is that any IdP with no mailbox-verification step it can point at will hit this, so the README documents the three causes an operator should triage (scope/mapping missing, user record has no address, verification explicitly false) rather than a vendor list. The fix belongs at the IdP: a custom scope mapping on the `email` scope returning the address with `email_verified` true, **replacing** the managed mapping on the provider rather than being added alongside it (two mappings would claim the same scope). The project README's OIDC section carries the blueprint snippet. This is not a papering-over: `email_verified` means "the asserting party checked", and it is load-bearing precisely because a first-seen identity links to any existing account holding the same verified email. Asserting it is honest for an IdP whose accounts are operator-created with no enrolment flow and no self-service email edit; it is dishonest for one with self-registration, where anyone could put a colleague's address on their own profile and inherit that colleague's WireMirage account. WireMirage keeps *requiring* the claim rather than growing a trust-unverified-email switch — the assertion belongs at the layer that actually knows how accounts come to exist.
- Cost: skipping ID-token validation forecloses public-client/implicit flows. Accepted — WireMirage is a confidential client, and those flows are discouraged by current OAuth guidance anyway. If a future need requires ID-token validation (e.g. claims only present in the token), it can be added without changing the config surface.
- The MCP-client Authorization Server ([0019-mcp-client-oauth.md](0019-mcp-client-oauth.md)) is unaffected — this is browser login only, feeding the same session machinery as GitHub and local auth.

**Alternatives considered:**

- **Treating "no allow rules" as trust-the-issuer** instead of the explicit `WM_OIDC_ALLOW_ALL` flag. Rejected: the same module config can point at a public issuer, where absent rules would silently admit the internet. The house rule is fail-fast over silent defaults; trusting the issuer is a one-line explicit declaration instead.

- **The `openidconnect` crate** (named in ADR-0010 as the intended tool). Rejected: the userinfo-based flow needs so little that the crate's large type-state API costs more to integrate than it saves, and the GitHub module already established the hand-rolled-on-reqwest house pattern — two structurally identical provider modules beat one hand-rolled and one framework-shaped. This is a recorded deviation from ADR-0010's implementation note; the decision it actually made (embedded RP, no sidecar IdP, operator allow rules) is unchanged.
- **ID-token (JWT) validation via JWKS** instead of userinfo. Rejected for v1: adds a JWT library plus key-cache/rotation machinery for no additional trust in the confidential-client code flow. Revisitable if a concrete IdP puts required claims only in the ID token.
- **Per-IdP adapters** (a `pocketid.rs`, a `keycloak.rs`, ...). Rejected: OIDC exists precisely so this isn't needed; adapter-per-provider is the GitHub-shaped exception, not the rule.
- **Multiple simultaneous OIDC issuers.** Deferred: config becomes a list, the login page grows N buttons, and the provider string needs namespacing (e.g. `oidc:{issuer-host}`) to keep `(provider, subject)` collision-free across issuers. No concrete user; the single-issuer config shape doesn't paint us into a corner.
- **Running a sidecar IdP** (Keycloak/Authentik) as the integration point. Already rejected in ADR-0010; nothing has changed — and ironically, this ADR is what lets users who *do* run those IdPs point WireMirage at them.

See also: [0010-oauth-oidc.md](0010-oauth-oidc.md), [0018-local-user-accounts.md](0018-local-user-accounts.md), [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md), [0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md), ../auth-and-authz.md, ../user-model.md.
