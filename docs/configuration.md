# Configuration reference

All configuration is via environment variables — there is no config file for
the host. The host fails fast on missing or contradictory required values
rather than silently falling back, so a misconfigured deploy surfaces at
startup instead of on the first failed request.

- [Storage](#storage-required)
- [Listener and apex host](#listener-and-apex-host)
- [Authentication paths](#authentication-paths)
  - [API tokens — bootstrap](#api-tokens--bootstrap)
  - [Browser login — OIDC](#browser-login--oidc-any-standards-compliant-idp)
  - [Browser login — GitHub OAuth](#browser-login--github-oauth)
  - [Browser login — local passwords](#browser-login--local-passwords-testing--trusted-networks-only)
- [Outbound callbacks / egress](#outbound-callbacks--egress)
- [Behind a reverse proxy](#behind-a-reverse-proxy)
- [Observability](#observability)
- [CLI configuration](#cli-configuration)

Authentication splits into **four independent paths** that can be enabled in
any combination:

| Path | Used by | Required env vars |
|---|---|---|
| **API tokens (bearer)** | `wm` CLI, MCP clients, scripts, agents | `WM_BOOTSTRAP_TOKEN` + `WM_BOOTSTRAP_EMAIL` (first start) |
| **OIDC** | Browser users with any OIDC IdP (Pocket ID, Keycloak, Authentik, Okta, …) | `WM_OIDC_ISSUER`, `WM_OIDC_CLIENT_ID`, `WM_OIDC_CLIENT_SECRET`, an allow posture, `SESSION_SECRET` |
| **GitHub OAuth** | Browser users | `WM_GITHUB_CLIENT_ID`, `WM_GITHUB_CLIENT_SECRET`, `WM_GITHUB_ALLOW_USERS` and/or `WM_GITHUB_ALLOW_ORGS`, `SESSION_SECRET` |
| **Local password** | Testing / trusted networks only ([ADR-0018](adr/0018-local-user-accounts.md)) | `WM_LOCAL_AUTH`, `SESSION_SECRET` |

API tokens always work. Mock traffic — everything served on a group
subdomain — is always unauthenticated by design; systems under test don't
carry credentials.

## Storage (required)

- `WM_STORAGE` — one of:
  - `memory` — in-process, state lost on restart. Fine for local development
    and integration tests.
  - `redis://host:port[/db]` — Valkey / Redis. The recommended deployment shape.
  - `rediss://host:port[/db]` — same, with TLS.

## Listener and apex host

- `WM_LISTEN_ADDR` — default `127.0.0.1:8080`. The release Docker image
  overrides this to `0.0.0.0:8080` so the container is reachable when
  published with `-p 8080:8080`. Deployments behind a reverse proxy should
  bind to `127.0.0.1` (see [deployment](deployment.md)).
- `WM_APEX_HOST` — the hostname this instance is reachable at, e.g.
  `wm.example.com`. It defines the **control-plane origin**; mock traffic is
  served on `{group}.{apex}` subdomains ([ADR-0030](adr/0030-virtual-host-routing.md)).
  Defaults to `localhost`, which is what you want for local development. A
  real deployment must set it — and must have wildcard DNS and a wildcard
  certificate for `*.{apex}`. See [deployment](deployment.md#dns-and-tls).

## Authentication paths

### API tokens — bootstrap

`WM_BOOTSTRAP_TOKEN=wmt_<some-secret>` plus `WM_BOOTSTRAP_EMAIL=<your-email>`
creates an admin user identified by that email on the very first host startup,
with the supplied plaintext as their API token. Both are required together
(accounts are keyed by email). Subsequent starts with the same env vars are
no-ops *as long as that user still exists*. Because the account is keyed by
your email, a later browser login (OIDC / GitHub) with the same verified email
lands in the *same* account — token and browser are one identity.

The host **refuses to start** when no users exist AND no login method is
configured at all — that prevents a fresh deployment from coming up
unreachable. "Login method configured" means *any* of: `WM_BOOTSTRAP_TOKEN` is
set, `WM_LOCAL_AUTH` is non-empty, or GitHub OAuth / OIDC is configured. A
deployment that wires up OAuth from day one doesn't need a bootstrap token —
the first browser login provisions the first user.

Generate one with `openssl rand -hex 32` (prefix with `wmt_` to match the
project's token convention). After first deploy, log in with this token via
the CLI (`WM_TOKEN=wmt_... wm health`), mint a real operator token
(`wm tokens create operator/default`), then revoke the bootstrap token
(`wm tokens revoke bootstrap`) so the env-var plaintext stops being a valid
credential.

**Drop `WM_BOOTSTRAP_TOKEN` after retirement.** The bootstrap check only asks
"does a user with `WM_BOOTSTRAP_EMAIL` exist?" — if the env vars stay set
after you delete that user, the *next* restart silently re-creates it with the
same token. Unsetting the env vars is part of retiring the account.

### Browser login — OIDC (any standards-compliant IdP)

One generic OIDC client covers every compliant provider — Pocket ID, Keycloak,
Authentik, Authelia, Zitadel, Dex, Okta, Auth0, Google, Microsoft Entra
([ADR-0035](adr/0035-generic-oidc-login.md)). Everything is driven by the
issuer's discovery document; there is no per-provider code.

At the IdP, register a **confidential** OAuth/OIDC client with the callback
URI `https://wm.example.com/auth/callback/oidc` — exact match, including the
path. PKCE (S256) is always sent.

- `WM_OIDC_ISSUER` — the issuer URL, e.g. `https://id.example.com`. Must be
  *exactly* what the IdP advertises in its discovery document
  (`{issuer}/.well-known/openid-configuration`); the host fetches that
  document at startup and refuses to start on a mismatch or an unreachable
  IdP.
- `WM_OIDC_CLIENT_ID` / `WM_OIDC_CLIENT_SECRET` — the registered client's
  credentials. Both or neither.
- Allow rules — **exactly one posture required**:
  - `WM_OIDC_ALLOW_ALL=true` — every user the issuer authenticates is allowed.
    The right posture for a **private IdP** (Pocket ID, closed-registration
    Keycloak, corporate Okta): you own the user table, so account existence
    already is the authorization decision. **Never** set this against a public
    issuer like Google — there "authenticated" means anyone on the internet.
  - Or per-identity rules (they OR together):
    - `WM_OIDC_ALLOW_EMAILS` — comma-separated exact emails.
    - `WM_OIDC_ALLOW_DOMAINS` — comma-separated email domains
      (`acme.example` allows `anyone@acme.example`).
    - `WM_OIDC_ALLOW_GROUPS` — comma-separated group names, matched against
      the IdP's groups claim. Emails marked `email_verified: false` never
      satisfy the email/domain rules.
  - Setting both `WM_OIDC_ALLOW_ALL` and per-identity rules refuses startup —
    the combination usually means the operator believes the rules still
    restrict something they don't.
- `WM_OIDC_ADMIN_EMAILS` / `WM_OIDC_ADMIN_GROUPS` — optional; matching users
  are promoted to admin on login (and demoted on the next login if removed).
- `WM_OIDC_DISPLAY_NAME` — optional login-button label (e.g. `Pocket ID`);
  defaults to the issuer's hostname.
- `WM_OIDC_GROUPS_CLAIM` — optional; the claim carrying group membership
  (default `groups`). The groups claim is the one non-standardized corner of
  OIDC — most self-hosted IdPs call it `groups`, but check yours.
- `WM_OIDC_EXTRA_SCOPES` — optional extra scopes appended to
  `openid profile email` (some IdPs only include groups when a dedicated scope
  is requested).
- `SESSION_SECRET` — see [GitHub OAuth](#browser-login--github-oauth).

Accounts are identified by the IdP's **verified email** claim. Returning
logins are matched on the stable `sub` claim, so an email change at the IdP
doesn't create a duplicate user, and the same verified email arriving via a
*different* provider links to one account.

Example — Pocket ID:

```sh
WM_OIDC_ISSUER=https://id.example.com \
WM_OIDC_CLIENT_ID=... WM_OIDC_CLIENT_SECRET=... \
WM_OIDC_ALLOW_ALL=true \
WM_OIDC_ADMIN_GROUPS=wiremirage-admins \
SESSION_SECRET=$(openssl rand -base64 48) \
  wm-host
```

#### When the IdP won't give you a verified email

Accounts are keyed on the verified email, so a login the IdP can't back with
one is refused:

```
OIDC login failed: the IdP returned no verified `email` claim, and
WireMirage accounts are keyed by email. Specifically: userinfo carried
no `email` claim — check that the client is granted an `email` scope
and that a claim mapping produces it.
```

That trailing sentence names which of three causes you hit (it's in the host
log too, alongside the `sub`). WireMirage treats an **absent**
`email_verified` as verified — plenty of IdPs omit the claim — so the three
are exhaustive:

1. **The client isn't granted the `email` scope**, or the IdP has no claim
   mapping producing it. Check the client's scopes/mappings, and that `email`
   shows up in `scopes_supported` at the discovery URL. (*"userinfo carried no
   `email` claim"*.)
2. **The user record has no email address.** Common for admin-created accounts
   and for users created by an LDAP/SCIM sync that didn't map a mail
   attribute. (*"the `email` claim was empty"*.)
3. **The IdP explicitly sent `email_verified: false`.** (*"the IdP marked the
   address `email_verified: false`"*.) Two shapes:
   - *Global* — the provider never asserts verification for anyone.
     **Authentik** is the confirmed case (below).
   - *Per-user* — the IdP has a verified flag that defaults to false for
     accounts an admin created directly. **Keycloak**'s per-user *Email
     verified* toggle behaves this way.

**Authentik**, concretely: its default `email` scope mapping hardcodes
`email_verified: False`, so a stock Authentik client fails **every** login.
Create a custom scope mapping (*Customisation → Property Mappings → Create →
Scope Mapping*) with scope name `email` and

```python
return {
    "email": request.user.email,
    "email_verified": bool(request.user.email),
}
```

then, on the provider, **replace** `authentik default OAuth Mapping: OpenID
'email'` with yours in *Selected Scopes* — don't add it alongside, or two
mappings claim the same scope. As a blueprint entry:

```yaml
  - model: authentik_providers_oauth2.scopemapping
    id: wm-scope-email
    identifiers:
      name: "WireMirage: OpenID 'email' (verified)"
    attrs:
      scope_name: email
      expression: |
        return {
            "email": request.user.email,
            "email_verified": bool(request.user.email),
        }
```

referenced from the provider's `property_mappings` as `!KeyOf wm-scope-email`.

**Before you assert verification anywhere, know what you're asserting.**
`email_verified` means "I checked that this person controls this mailbox", and
the claim is load-bearing: WireMirage links a first-seen identity to any
existing account carrying the same verified email. Asserting it is honest when
*you* control who gets an address — admin-created accounts, an authoritative
directory sync, no self-service email edit. It is **not** honest on an IdP
with self-registration or a user-editable email field: there, anyone could put
a colleague's address on their profile and inherit that colleague's account.

### Browser login — GitHub OAuth

Register a GitHub **OAuth App** — Settings → Developer settings → **OAuth
Apps** → New OAuth App. Not a *GitHub App*: that's the richer system for apps
acting on repos (webhooks, permissions, installation flow), none of which
applies to reading a user's identity.

- **Homepage URL**: `https://wm.example.com` — your public URL.
- **Authorization callback URL**: `https://wm.example.com/auth/callback` —
  exact match, including the path. The host computes this URL from the inbound
  request's `X-Forwarded-*` headers, so it must agree with what GitHub sees.
  A mismatch surfaces as `redirect_uri mismatch` from GitHub.
- **Enable Device Flow**: leave unchecked.

Then set:

- `WM_GITHUB_CLIENT_ID` — the OAuth app's Client ID.
- `WM_GITHUB_CLIENT_SECRET` — the generated secret. Treat as a credential.
- `WM_GITHUB_ALLOW_USERS` — comma-separated GitHub logins allowed to log in.
  OR'd with `WM_GITHUB_ALLOW_ORGS`.
- `WM_GITHUB_ALLOW_ORGS` — comma-separated GitHub org logins; any member of
  any listed org is allowed in. Requires the `read:org` scope, which the host
  always requests.
- `WM_GITHUB_ADMIN_USERS` — optional subset of allowed users promoted to admin
  on first login. When empty, every GitHub user lands as a non-admin and
  existing admins promote via `wm users update <email> --admin`.
- `SESSION_SECRET` — HMAC key for the `wm_session` and `wm_csrf` cookies. At
  least 32 bytes; `openssl rand -base64 48` works. Rotating it invalidates
  every existing session, so keep it stable unless you mean to log everyone
  out.

If one of the client ID / secret pair is set and the other isn't, the host
refuses to start — a half-configured OAuth path is a silent footgun. If
neither is set, the GitHub flow simply isn't enabled and the login page omits
the button. GitHub logins also require a verified email on the account
([ADR-0036](adr/0036-email-only-identity.md)).

### Browser login — local passwords (testing / trusted networks only)

- `WM_LOCAL_AUTH=alice@corp.example:hunter2:admin,bob@corp.example:pw` —
  comma-separated `email:password[:role]` triples; `role` is `admin` or
  omitted. The identifier must be an email (accounts are keyed by email;
  nothing is ever sent to it). Passwords are argon2id-hashed at startup; the
  plaintext is never persisted.
- `SESSION_SECRET` as above.

This mode exists for testing and trusted-network deployments — passwords in
env vars aren't OAuth-grade. Don't expose a host with `WM_LOCAL_AUTH` set to
the public internet without a TLS edge **and** an IP allow-list at the proxy.

## Outbound callbacks / egress

Handlers can schedule outbound webhooks with `host.scheduleCallback(...)` —
the host fires the request once, after the mock's response is sent. This is
the **only** network egress out of the otherwise-closed sandbox
([ADR-0034](adr/0034-outbound-callbacks.md)), so it's off by default:

- `WM_EGRESS` — set to `on` / `true` / `1` to enable callbacks host-wide.
  Unset (default) → `scheduleCallback` is rejected with a catchable error.
- `WM_EGRESS_ALLOW` — comma-separated IPv4/IPv6 CIDRs (or bare IPs) that
  **override** the hardcoded special-use default-deny. Even with egress on,
  loopback, link-local (including the `169.254.169.254` cloud-metadata IP),
  private, CGNAT, ULA, and multicast ranges are denied unless allow-listed.
  The usual self-hosted/CI need is to allow the internal range the SUT lives
  on, e.g. `WM_EGRESS_ALLOW=10.0.0.0/8`.
- `WM_EGRESS_DENY` — extra ranges to block, for stricter operators.

On top of the host capability, **each group opts in** via its
`callout_enabled` flag (`wm groups update <group> --callout`, the
`update_group` MCP tool, or the group-detail UI). The decision is enforced on
the **resolved IP**, redirects aren't followed, and delivery is single-attempt
best-effort. Outcomes land in a per-group callback journal.

## Behind a reverse proxy

- `WM_TRUSTED_PROXY=<hostname>` (comma-separated for several) — declares that
  wm-host sits behind a trusted, HTTPS-terminating reverse proxy serving that
  hostname, and turns on the whole posture together
  ([ADR-0027](adr/0027-single-trusted-proxy-switch.md)):
  - appends `Secure` to the `wm_session` / `wm_csrf` cookies;
  - trusts `X-Forwarded-For` (per-IP login throttle) and `X-Forwarded-Proto` /
    `-Host` (OAuth redirect-URI derivation);
  - adds the hostname(s) to the MCP `Host`-header allowlist, on top of the
    localhost defaults.

Leave it unset for local HTTP development. It's one setting so the proxy
posture can't be half-configured. Details and the rest of the hardening
checklist are in [deployment](deployment.md).

**MCP and the `Host` allowlist.** The streamable-HTTP MCP transport allowlists
the `Host` header as DNS-rebinding protection (defaults: `localhost`,
`127.0.0.1`, `::1`). Behind a proxy, a request arriving with
`Host: wm.example.com` is otherwise rejected *before the auth middleware
runs*, which MCP clients report as an opaque "authorization failed". If a
remote MCP client fails to connect with a valid token, check that
`WM_TRUSTED_PROXY` includes the hostname.

## Observability

- `OTEL_EXPORTER_OTLP_ENDPOINT` — URL of an OTLP/gRPC collector (e.g.
  `http://localhost:4317`). When unset, the host logs to stderr only; when
  set, **both traces and metrics** are exported. There is no localhost
  fallback — the absence of an endpoint is taken as "don't try".
  ([ADR-0017](adr/0017-observability-tracing.md) for traces,
  [ADR-0024](adr/0024-metrics-via-otlp.md) for metrics.)
- `OTEL_SERVICE_NAME` — default `wm-host`.
- `OTEL_RESOURCE_ATTRIBUTES` — standard OTel SDK behaviour; comma-separated
  `key=value` pairs.
- `OTEL_METRIC_EXPORT_INTERVAL` — standard OTel SDK env var, milliseconds
  between metric pushes. Default 60000.

What to watch, and which signal answers which question, is in
[observability](observability.md).

## CLI configuration

The `wm` CLI reads its host and token from flags, environment, or a config
file, in that order of precedence:

- `--host` / `WM_HOST` — base URL of the control plane (default
  `http://localhost:8080`).
- `--token` / `WM_TOKEN` — bearer token.
- `--profile` / `WM_PROFILE` — named profile from the config file (default
  `default`).
- `WM_CONFIG_FILE` — override the config file location; otherwise
  `$XDG_CONFIG_HOME/wiremirage/config.toml`, else
  `~/.config/wiremirage/config.toml`.

```toml
# ~/.config/wiremirage/config.toml
[profiles.default]
host = "http://localhost:8080"
token = "wmt_dev_local"

[profiles.staging]
host = "https://wm.example.com"
token = "wmt_..."
```

`wm --profile staging groups list`. A missing config file is fine, and so is
an absent `default` profile — but a profile you name explicitly and that isn't
in the file is an error listing the profiles that are.
