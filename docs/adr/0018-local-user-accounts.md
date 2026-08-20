# ADR-0018: Local user accounts via env var

**Status:** Accepted

**Context:** [0010-oauth-oidc.md](0010-oauth-oidc.md) rejected local username/password authentication as the primary auth strategy: real password handling — verification emails, password reset, MFA, lockout, breach handling — is a substantial body of work that delegating to OAuth providers avoids entirely. That decision still stands for the primary path.

But it leaves two real use cases unserved:

1. **Testing.** A developer running WireMirage locally for `cargo test` or an integration test in CI doesn't want to register an OAuth client just to log into the UI. A token-only mode works for the API but means the UI is unreachable by browser without a proper login flow.

2. **Small / intranet deployments.** A team running WireMirage on a private network for ad-hoc shared use shouldn't have to stand up an OAuth client either. Pasting a bearer token into a browser address bar isn't a real login UX, and the existing `WM_BOOTSTRAP_TOKEN` admin gives API access but no session-cookie path.

Both cases share a property: the deploying operator has full administrative access to the host's configuration, and is willing to trust that anyone with config access is trusted with credentials. Under that constraint, the heaviest costs of local password auth (password reset, breach handling, MFA) don't apply — there is no separate "user" to recover credentials on behalf of, no public registration, and no expectation of confidentiality between operator and end user.

**Decision:** Add a deliberately-scoped local-user mechanism that exists *alongside* the OAuth flow, not in place of it. Users are defined entirely in a single environment variable, with plaintext passwords; the host argon2id-hashes them on startup, keeps the hashes in memory, and serves a username/password form on the UI's login screen when this mode is configured.

**Hashing note (slice 20 implementation, 2026-05):** The original draft of this ADR specified bcrypt as the in-memory hashing function. Implementation chose argon2id instead — OWASP currently recommends Argon2id for new password storage, and the rationale for sticking with bcrypt was carry-over from the rejected alternative below ("Pre-hashed passwords in the env var.") rather than a positive choice. argon2id gives stronger resistance to GPU brute force and the PHC-encoded hash format embeds the cost parameters so we can tune them later without breaking existing hashes. Wire format and threat model are otherwise unchanged.

```
WM_LOCAL_AUTH=alice:hunter2:admin,bob:correct-horse-battery-staple
```

Each entry is `username:password:role`, comma-separated. Role is `admin` or `user`, defaulting to `user`. The host parses on startup, argon2id-hashes the passwords, never persists plaintext.

User records (id, name, identities, is_admin, timestamps) are persisted to Valkey identically to OAuth users — the `provider` value on the identity is `"local"`, the `subject` is the username. Password hashes are **not** persisted; they live in an in-memory `LocalAuth` map rebuilt from `WM_LOCAL_AUTH` at each startup.

Login mints a session cookie via the same machinery OAuth uses; downstream authorization checks can't tell the two apart.

**Consequences:**

- **No password reset, no signup, no MFA, no breach response.** All deliberately excluded. If a user forgets their password, the operator edits the env var and restarts. The feature is for cases where this restart-driven workflow is acceptable.
- **No public exposure intended.** The threat model assumes a trusted network (LAN, VPN, dev-laptop loopback, container-cluster-internal). Operators who need real security should use OAuth and not configure `WM_LOCAL_AUTH`.
- **Plaintext in process environment.** `WM_LOCAL_AUTH` is visible to anything that can read the host's env: container metadata, `/proc/{pid}/environ` for processes on the same machine, supply-chain attacks on the host binary. This is the explicit cost. Hashing on startup limits in-memory exposure but doesn't change the env-var exposure surface.
- **Admin role lives in the env var.** A local user's admin flag comes from the `:admin` suffix, not from the `auth.admins` config list. Local users and OAuth users get their admin status from different sources; this is the consistency cost of keeping each user's full config in one place.
- **User records persist after config removal.** Removing a name from `WM_LOCAL_AUTH` blocks their next login but doesn't auto-delete the existing user record, their routes, or their tokens. Same semantics as removing an OAuth user from an allow-list.
- **Rate limiting on the login form is in-scope for v1.** A modest per-IP throttle on `POST /__auth/login/password` (e.g., 5 failed attempts in 60s → 60s lockout, in-memory counter) prevents drive-by brute force. Doesn't make the feature safe to expose publicly; does prevent trivial attacks within the trusted-network threat model.
- **Cost: documentation.** The README, deployment guide, and operator docs need to clearly mark this feature as "for testing and small/private deployments" rather than as a general-purpose auth option. Mis-deploying it as the primary auth on a public host would be a real risk.

**Alternatives considered:**

- **Pre-hashed passwords in the env var.** Operators run `wm hash-password hunter2` once, paste the hash into `WM_LOCAL_AUTH=alice:$2b$12$xxx:admin`. Slightly better in-memory posture (the host never sees plaintext). Rejected: the env var is the exposure surface anyway, the hashing step adds operator friction, and the in-memory hashing-on-startup approach gets most of the benefit for free. We could still support pre-hashed values later by detecting the `$2b$` prefix; not in v1.
- **Configuration file (`auth.toml` or similar) for local users.** Avoids env-var quoting issues if passwords contain `:` or `,`. Rejected for now in favor of env-var simplicity; the format is restrictive but adequate for the testing / small-deployment use case. If a config-file path becomes useful (e.g., for many users), it can be added as `WM_LOCAL_AUTH_FILE=/path/to/users.toml` without breaking the env-var form.
- **Magic-link login (one-time link emailed to the user).** Solves the no-password-storage goal differently. Rejected because it requires an email-sending pipeline (or a third-party service) and adds dependencies. Doesn't fit the "testing on localhost" case at all.
- **Browser session via bearer token.** Operator distributes a bearer token; UI accepts it as a session credential. Rejected because tokens and sessions are different concepts (per [0012-api-tokens.md](0012-api-tokens.md)), and conflating them at the credential level confuses operator and user mental models. Tokens stay for programmatic access; sessions stay for browser access; OAuth or local-auth mints sessions.
- **Defer entirely; require OAuth always.** The original ADR-0010 stance. Rejected because the cost of registering an OAuth client for a 10-minute test run, or for a 3-person intranet deployment, is real friction the original ADR underweighted.

**Relationship to [0010-oauth-oidc.md](0010-oauth-oidc.md):** This ADR does not supersede ADR-0010. The decision in ADR-0010 — OAuth as the primary, recommended path — remains correct. This ADR carves out a deliberately-scoped escape hatch for cases ADR-0010's reasoning doesn't address. Both ADRs are active.

See also: ../auth-and-authz.md, ../user-model.md, [0011-route-ownership.md](0011-route-ownership.md), [0012-api-tokens.md](0012-api-tokens.md).
