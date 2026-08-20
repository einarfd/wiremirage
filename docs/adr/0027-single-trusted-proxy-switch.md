# ADR-0027: One `WM_TRUSTED_PROXY` switch for behind-a-TLS-edge deployment

**Status:** Accepted

**Context:**

Running wm-host behind a reverse proxy — the production shape (Caddy/ALB terminating TLS, forwarding to the host on loopback) — currently requires **three independent env knobs**, all added piecemeal (slice 44 + the MCP transport work):

- `WM_SECURE_COOKIES=1` — append `Secure` to the `wm_session` / `wm_csrf` cookies.
- `WM_TRUST_FORWARDED_HEADERS=1` — honor `X-Forwarded-For` (login-throttle IP) and `X-Forwarded-Proto`/`-Host` (OAuth redirect-URI derivation).
- `WM_MCP_ALLOWED_HOSTS=<host>` — add the public hostname to rmcp's `Host`-header allowlist (DNS-rebinding protection).

These are three facets of **one fact** — "I'm behind a trusted TLS-terminating proxy serving hostname H" — yet each is set separately, and **forgetting any one is a different, partly-silent failure**:

- no `WM_SECURE_COOKIES` → cookies work but aren't `Secure` (silent security downgrade);
- no `WM_TRUST_FORWARDED_HEADERS` → OAuth callback URL is derived from the wrong scheme/host → **browser login breaks**;
- no/stale `WM_MCP_ALLOWED_HOSTS` → MCP is rejected **before auth** with an opaque "authorization failed".

It's worse in the deployment: the MCP hostname is a hand-maintained literal that must be kept in sync with the proxy's vhost (which derives from `nixos/vars.nix`), so changing the domain moves the proxy but silently strands MCP. This is a deployment footgun with no upside — the three are never meaningfully configured independently.

**Decision:**

Replace the three with a **single `WM_TRUSTED_PROXY`** whose value is the public hostname(s) the trusted edge serves (comma-separated). When set, it implies all three behaviors:

- trust `X-Forwarded-*` (throttle IP + OAuth proto/host),
- emit `Secure` on session / CSRF cookies,
- add the hostname(s) to the MCP rmcp allowed-hosts, **on top of** the retained `localhost` / `127.0.0.1` / `::1` defaults.

Unset (or empty) = direct-exposure defaults: no forwarded trust, no `Secure`, MCP localhost-only. **Clean break** — the three old vars are removed, not kept as overrides (pre-1.0; the sole deployment is updated in lockstep; matches the project's "no decorative back-compat" rule).

Internally, the `AppState` fields (`secure_cookies`, `trust_forwarded_headers`, and a new `mcp_allowed_hosts`) are unchanged in how they're *consumed* — only the env→state wiring in `main.rs` collapses to one variable, and the MCP allowlist moves from an `env::var` read at transport-build time to an `AppState` field. The deployment single-sources the value from `vars.nix:hosts.wm`, so the hostname is declared **once** and feeds both Caddy and the app.

**Consequences:**

- **"Behind a TLS edge" becomes one setting that can't be half-configured** — the partial-failure footgun is gone, and the host-side hostname is single-sourced.
- **MCP DNS-rebinding protection is preserved, not weakened**: the configured host is allowlisted and the localhost defaults remain. We didn't take the "disable the check when proxied" shortcut — we have the hostname anyway, so allowlisting it is free.
- **The cookie / throttle / OAuth code is untouched** — same `AppState` getters, same behavior; only the population path changed.
- **Cost: a breaking config change.** The only live deployment (hetzner) is updated in the same change. A *plain-HTTP* reverse proxy (no TLS) is off the happy path — `WM_TRUSTED_PROXY` implies HTTPS at the edge (it turns on `Secure` cookies); that's a documented non-supported shape, not a silent trap.

**Alternatives considered:**

- **Keep three vars, document better.** Rejected: docs don't remove the footgun — you can still set two of three.
- **One boolean + a separate hostname var.** Rejected: that's two vars again (the hostname is the only non-boolean), reintroducing the keep-in-sync problem. The switch should *carry* the hostname.
- **Disable the MCP rebinding check entirely when proxied** (so no hostname needed at all). Rejected: needlessly weakens the in-app guard; allowlisting the known host costs nothing and keeps the protection.
- **Keep the old vars as overrides on top of the switch.** Rejected: reintroduces the surface it set out to shrink, and conflicts with the no-back-compat-shims convention; clean break is simpler pre-1.0.

**See also:** [0010-oauth-oidc.md](0010-oauth-oidc.md), [0018-local-user-accounts.md](0018-local-user-accounts.md), [0019-mcp-client-oauth.md](0019-mcp-client-oauth.md), ../storage-model.md
