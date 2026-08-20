# ADR-0010: OAuth 2.0 / OIDC for authentication

**Status:** Accepted

**Context:** WireMirage is intended to be reachable from the public internet (the SUT calls the public listener; the developer logs into the UI from their laptop or office). Without authentication, anyone who finds the URL could create routes, inspect journals, or exhaust resources.

The expected user population is small teams of engineers who already use Google Workspace, GitHub, or both. The realistic options for authenticating those users:

1. **Local username/password** with our own user store. Real work — verification emails, password reset, MFA, lockout, breach handling. Adds zero unique value.
2. **OAuth 2.0 with social providers** (Google, GitHub). Standard, well-trodden, no password handling on our side.
3. **Enterprise SSO** (SAML, OIDC against a corporate IdP). Too heavyweight for the project's target audience and operational scope.
4. **Bearer-token-only** (no human login at all). Workable for headless deployments but rules out the UI use case.

**Decision:** OAuth 2.0 with OIDC where the provider supports it (Google, Microsoft, Okta, Auth0, ...). GitHub is supported via OAuth 2.0 + GitHub-specific user/org APIs since GitHub isn't strictly OIDC. Provider configuration is per-deployment; the operator picks which providers are enabled and how identities are accepted.

The host implements OAuth/OIDC directly using the `openidconnect` Rust crate; we do not run a separate IdP service like Authentik or Keycloak.

**Consequences:**

- **No password handling.** All credential management is delegated to providers. We never see, store, or transmit passwords.
- **The provider is the strong identity claim.** "This person logged in with this Google account" or "this person is a member of this GitHub org" are claims we trust the provider to make accurately.
- **Operator-configured allow rules.** Providers vouch for identity; the operator decides which identities are allowed. Default config supports email-domain allow-lists for Google and org-membership allow-lists for GitHub. See ../auth-and-authz.md.
- **No additional service to run.** The host handles the OAuth flow itself, exchanging codes for tokens and fetching user identity via the provider's UserInfo endpoint. One Rust crate dependency, no extra container.
- **Multiple providers can be configured.** A user logging in with Google or GitHub gets different identity records, but we link them by email when both providers' allow rules let them in. (The linking model is in ../user-model.md.)
- **Cost: OAuth client credentials need to be obtained.** The deploying team registers a Google OAuth client (low friction in any Google Workspace) and/or a GitHub OAuth app. This is operationally light but it's the one gating step before deployment.
- **Cost: bearer tokens for the API and MCP need a separate path.** OAuth provides session-cookie authentication for the UI but isn't appropriate for long-lived programmatic access. API tokens fill that gap; see [0012-api-tokens.md](0012-api-tokens.md). **Interactive native MCP clients (Claude Desktop, Cursor, MCP Inspector) are a separate case** — they speak the MCP-spec OAuth dance, which WireMirage handles via a dedicated Authorization Server scoped to `/__api/mcp`. See [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md).
- **Cost: provider outage means new logins fail.** If Google is down, no one can log in with Google. Existing sessions continue to work; existing API tokens continue to work. This is the standard trade-off of delegated authentication.

**Alternatives considered:**

- **Authentik or Keycloak as a sidecar IdP.** Both are excellent products for organizations that need a central IdP managing many applications. Overkill for WireMirage — we're a single application, we don't need user-level federation, we don't need SCIM provisioning, we don't need a custom login UI. Embedded OIDC client is meaningfully simpler.
- **GitHub-only.** Tempting because GitHub org membership is a stronger trust claim than email domain. Rejected because many teams have non-engineering users who may need access without a GitHub account, and constraining to GitHub closes that door.
- **Magic-link login (no provider).** We send an email with a one-time login link. Lighter than full OAuth but requires running an email-sending pipeline (or trusting a third-party email-sending service). More moving parts than OAuth for the same result. Rejected.
- **Bearer-token-only, with the operator distributing tokens manually.** Works for headless operation but a UI without login is unusable from a browser. Rejected.

**Provider list at v1:** GitHub only. Other providers ship on demand.

- **GitHub** (OAuth 2.0 + custom user-info via `api.github.com/user`) is the v1 provider. Org-membership allow-rules cover the small-team trust shape; app registration at github.com/settings/developers is one of the lightest OAuth onboarding flows.

Adding **Google** (OIDC, `https://accounts.google.com`) or any other OIDC-compliant provider (Microsoft, Okta, Auth0, generic OIDC) is configuration-only — drop credentials and an issuer URL into the auth config. [0035-generic-oidc-login.md](0035-generic-oidc-login.md) delivers that generic OIDC provider (with one recorded deviation: hand-rolled on reqwest like the GitHub module, not the `openidconnect` crate named above). Adding another non-OIDC OAuth provider (Bitbucket, GitLab) requires a small adapter for that provider's user-info endpoint; we'll ship one if there's demand.

The narrowing from "Google + GitHub at v1" (in the original draft of this ADR) to "GitHub only at v1" reflects the initial deployment shape: a single operator with a GitHub account, with the rest of the multi-provider machinery staying in the design so it's ready to land when a second provider has a concrete user. The `openidconnect` Rust crate is still the right tool — the embedded-client decision and the allow-rule model are unchanged.

See also: ../auth-and-authz.md, ../user-model.md, [0011-route-ownership.md](0011-route-ownership.md), [0012-api-tokens.md](0012-api-tokens.md), [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md).
