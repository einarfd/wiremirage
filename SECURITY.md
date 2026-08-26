# Security policy

## Reporting a vulnerability

Please report security issues privately, **not** through the public issue
tracker. Use GitHub's [private vulnerability
reporting](https://github.com/einarfd/wiremirage/security/advisories/new) on
this repository.

This is a single-maintainer project: expect an acknowledgement within a few
days, and a fix timeline that depends on severity. There is no bounty
programme. Credit in the advisory is offered unless you'd rather not have it.

## What's in scope

Anything that lets someone cross a boundary WireMirage claims to hold:

- **Sandbox escape** — a handler reading or writing outside its wasm sandbox,
  reaching the host filesystem or network, or breaking out of the fuel /
  epoch / memory limits in a way the host doesn't trap.
- **Egress bypass** — an outbound callback reaching an address the egress
  policy should have denied (loopback, link-local including
  `169.254.169.254`, private ranges, or an operator's `WM_EGRESS_DENY` entry),
  including via DNS rebinding or redirects.
- **Cross-tenant access** — reading or modifying another user's routes,
  groups, state, or journal entries; a group's routes answering on another
  group's subdomain.
- **Authentication and session flaws** — token forgery, session cookie
  forgery, CSRF on an authenticated UI form, the OAuth/OIDC flows accepting an
  identity they shouldn't (in particular anything that lets an attacker link
  to an existing account without a genuinely verified email).
- **Privilege escalation** — a non-admin reaching an admin-only surface.

## What's not

These are documented behaviours, not vulnerabilities:

- **Mock traffic is unauthenticated.** Any route registered on an instance is
  reachable by anyone who can reach its subdomain. Systems under test don't
  carry credentials; that's the point. Don't mock anything whose responses are
  sensitive.
- **Handlers run user-supplied code by design** — inside the sandbox. The
  sandbox holding is in scope; the ability to run code in it is the product.
- **`WM_LOCAL_AUTH` is passwords in environment variables**
  ([ADR-0018](docs/adr/0018-local-user-accounts.md)) — for testing and trusted
  networks only, and documented as such.
- **Direct exposure without `WM_TRUSTED_PROXY`** — running the host on a
  public interface with no TLS edge is a misconfiguration; see
  [deployment](docs/deployment.md#production-hardening).
- **Resource exhaustion by an authenticated user** on their own instance.
  There's no per-tenant concurrency quota yet
  ([ADR-0032](docs/adr/0032-sandbox-limits-multi-tenant.md) covers what exists
  and what doesn't).

## Supported versions

Pre-1.0: only `main` is supported. Fixes land there first and reach the
`:main` container image on the next push. The `:latest` / `:0.1` tags track
the newest *release*, so they don't carry a fix until it is tagged — for a
security fix that is same-day. If you need a fix before then, run `:main` or
the `:sha-` tag naming the fixing commit.
