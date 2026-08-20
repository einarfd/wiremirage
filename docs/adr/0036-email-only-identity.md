# ADR-0036: Email-only identity — the verified email is the account

**Status:** Accepted (2026-07)

## Context

The user model grew up GitHub-shaped: every account carried a unique `name` handle (the GitHub login / OIDC `preferred_username`) alongside an optional `primary_email`. Two earlier passes moved identity toward email — cross-provider linking by verified email (user-model.md as-built), then email-primary (unique `user:by-email` index, email accepted as the admin-surface selector, and a derived-handle scheme so a taken username never blocked a login: `alice` → `alice-2`).

Dogfooding the multi-IdP deployment (GitHub + Authentik via ADR-0035) surfaced the remaining problem: the username still existed, and it was what users *saw*. The operator's position was blunt and correct — usernames are not unique across IdPs and were never the identity key, so displaying and addressing accounts by them is misleading; emails are unique by definition and are what let multiple IdPs coexist. The derived-handle machinery (`einarfd-2`) existed solely to keep a field alive that carried no information anyone wanted.

There is also no product pressure for usernames: WireMirage sends no email (operator constraint: no email integration, ever — the address is an identifier, not a channel), has no public profiles, and its user set is a small allow-listed team.

## Decision

The **verified email is the account** — its unique identifier and its display label, on every surface. `User.name` is deleted; the record is `{ id, email, is_admin, created_at }`.

Concretely:

- **One index.** `user:by-email:{email}` is the only user index (unique, self-healing for pre-index records). `user:by-name` is retired. `(provider, subject)` via `user:by-identity:*` remains the login-time identity primitive; the email remains the cross-provider join key (linking semantics unchanged from user-model.md).
- **Display = email.** UI owner columns and the signed-in badge, the users CLI, MCP `who_am_i` / `summarize_workspace`, and REST user records all show the email. No display-name field exists; provider name claims are ignored. (A cosmetic display name could return later; it would never be an identifier.)
- **Selector = email.** The REST user endpoints and the users CLI address a user by their email. The `@`-discrimination logic from the email-primary pass is gone.
- **No verified email → no login.** An OAuth/OIDC identity whose provider supplies no verified email is refused with a clear error, instead of being provisioned under a username. An account cannot exist without its key, and this hardens the linking model (every account is reachable by exactly one verified email).
- **Local auth is email-keyed.** `WM_LOCAL_AUTH` identifiers must be email-shaped (fail-fast at startup); the login form field is "Email"; the `local:` identity subject is the email. A local entry whose email matches an OAuth-provisioned account logs into that account — the same linking contract as providers.
- **Bootstrap requires an email.** `WM_BOOTSTRAP_TOKEN` now requires `WM_BOOTSTRAP_EMAIL` alongside (fail-fast when missing). The bootstrap admin is keyed by the operator's email, so a later browser login with the same verified email lands in the *same* account — bootstrap token and browser session are one identity, which also makes "rotate the bootstrap token" the retirement step rather than "delete the bootstrap user".
- **Derived handles are deleted.** `derive_unique_name`, the `-N` suffixing, and `NameTaken`-on-login are removed (they had shipped only weeks earlier in the email-primary pass). `NameTaken` survives only where a human explicitly types a name: token names.

### Migration (in place, no rewrite)

- The storage field name stays `primary_email`; existing records need no migration.
- A record that predates email-only identity decodes its stored legacy `name` as its identifier until a login (OAuth backfill) or bootstrap adopts it; legacy bare handles are matched by scan but never written into the email index.
- A legacy `bootstrap` record is adopted on the next restart with both bootstrap env vars set: it gains the email as its identifier and its existing token keeps working.

## Consequences

- Users see `einar@example.com` everywhere, never `einarw` or `einarw-2`. Mixing IdPs (the actual deployment shape: GitHub for some users, a private IdP for others) produces one account per human with no name-collision caveats.
- Net code deletion: the collision matrix, handle derivation, name index, and selector discrimination all went away; the auth layer treats the email as an opaque case-normalized unique string, with email-shape enforcement at the input boundaries (API create, env parsing, OAuth callbacks).
- Breaking, absorbed by the operator (pre-1.0 stance): `WM_LOCAL_AUTH` entries and the login form field changed shape; REST/MCP user records replaced `name` + `primary_email` with `email`; user creation takes an email; `WM_BOOTSTRAP_EMAIL` is newly required *when* `WM_BOOTSTRAP_TOKEN` is set (deployments where users already exist boot unchanged).
- An IdP that withholds email (or marks it unverified) cannot log users in. Accepted: the fix belongs at the IdP (include a verified email in userinfo), and silently minting unlinked, email-less accounts was the worse failure mode.
- Email rotation at a provider still never re-keys an account automatically (set-once, unchanged from user-model.md). An admin-driven "change my email" flow remains future work — it now implies re-keying the identifier, not just a profile field.

## Alternatives considered

- **Keep the username as display label, email as key** (the email-primary status quo). Rejected: two name-like fields confuse which one is identity; the derived-handle scheme leaked meaningless labels (`alice-2`) into every surface; and it existed only as an artifact of the GitHub-first history.
- **Username with a separate optional display name from IdP claims.** Rejected for the same reason plus staleness: claims-sourced labels drift, and no surface needed them.
- **Allow email-less accounts to keep working for providers without verified emails.** Rejected: such accounts can never link, collide silently with future logins, and contradict "the email is the key". Refusing with an actionable error is honest.
- **Retrofit this into ADR-0010/0018/0035.** Rejected: the decision spans all three (OAuth, local auth, OIDC) and none of them owns the identity model; a dedicated record keeps their histories clean.

## See also

- ../user-model.md — the identity model this ADR amends (as-built notes updated in the same pass)
- ../auth-and-authz.md — login flows, linking steps, local-auth operator detail
- [0010-oauth-oidc.md](0010-oauth-oidc.md), [0018-local-user-accounts.md](0018-local-user-accounts.md), [0035-generic-oidc-login.md](0035-generic-oidc-login.md) — the login-path ADRs this decision cuts across
