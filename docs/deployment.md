# Deployment

WireMirage is a single binary plus a Valkey instance. This page covers running
it for real: the DNS and TLS shape virtual-host routing needs, the container
image, and the hardening checklist.

For the full env-var reference see [configuration](configuration.md).

## The routing shape you have to plan for

Mock traffic is served on **per-group subdomains** — a group named
`stripe-mock` on an instance whose apex is `wm.example.com` serves its routes
at `https://stripe-mock.wm.example.com/...`
([ADR-0030](adr/0030-virtual-host-routing.md)). The apex itself is
**control-plane only**: `/api/*`, `/ui/*`, `/auth/*`, `/health`, `/ready`. It
serves no mock traffic, and an unmatched apex path is a plain 404 that is
deliberately not journaled.

Consequences for a deployment:

- You need **wildcard DNS** — an `A`/`AAAA` record for `wm.example.com` and
  one for `*.wm.example.com`.
- You need a **wildcard TLS certificate** for `*.wm.example.com`. Public CAs
  only issue those over the **DNS-01** challenge, so your ACME client needs
  API credentials for the zone. HTTP-01 will not work for the wildcard.
- Set `WM_APEX_HOST=wm.example.com` so the host knows which label is the
  group and which requests are control-plane.
- Set `WM_TRUSTED_PROXY=wm.example.com` (see
  [hardening](#production-hardening)). Include any other hostnames the edge
  serves.

Locally none of this applies: the default apex is `localhost`, and
`{group}.localhost` needs no DNS because the `Host` header alone drives group
resolution:

```sh
curl -H 'Host: demo.localhost' http://localhost:8080/v1/charges
```

### DNS and TLS

Whatever proxy you run, it has three jobs: terminate TLS for the apex **and**
the wildcard, pass the original `Host` through untouched (the host resolves
the group from it), and set `X-Forwarded-Proto` / `-Host` / `-For`.

The shortest complete example is Caddy — one site block covers both:

```caddyfile
*.wm.example.com, wm.example.com {
    tls {
        dns <your-provider> {env.DNS_API_TOKEN}
        propagation_delay 2m      # some providers converge slowly
    }
    reverse_proxy 127.0.0.1:8080
}
```

(`dns` needs a Caddy build that includes your DNS provider's plugin; the
stock binary has none.)

Traefik is what the maintainer's own deployment runs. It needs the wildcard
spelled out on the router — without `tls.domains` it tries to mint a
certificate per hostname, which is the failure you hit the first time a group
subdomain is requested:

```yaml
# dynamic config
http:
  routers:
    wm-apex:
      rule: "Host(`wm.example.com`)"
      entryPoints: [websecure]
      service: wiremirage
      tls: { certResolver: myresolver }
    wm-groups:
      rule: 'HostRegexp(`^[a-z0-9-]+\.wm\.example\.com$`)'
      entryPoints: [websecure]
      service: wiremirage
      tls:
        certResolver: myresolver
        domains:
          - main: "*.wm.example.com"
  services:
    wiremirage:
      loadBalancer:
        servers:
          - url: "http://wiremirage:8080"
```

```yaml
# static config
certificatesResolvers:
  myresolver:
    acme:
      dnsChallenge:
        provider: <your-provider>
        delayBeforeCheck: 120s    # same propagation problem, different knob
```

**Budget for DNS propagation.** Both examples carry a deliberate delay before
the ACME check. Some providers (Hetzner among them) converge across their
nameservers slowly enough that the challenge is verified against a stale
record and the issuance fails — with an error that points at the challenge,
not at DNS. If wildcard issuance fails on the first try and succeeds on a
retry, that's this.

## Running the container

CI builds the release image from the repo-root `Dockerfile` and publishes a
multi-arch manifest to `ghcr.io/einarfd/wiremirage`. The tags:

| Tag | Means | Published by |
|---|---|---|
| `:latest` | newest stable release | tagging `vX.Y.Z` |
| `:0.1` | newest `0.1.x` release | tagging `vX.Y.Z` |
| `:0.1.0` | that exact release, immutable | tagging `vX.Y.Z` |
| `:main` | tip of `main`, unreleased | every push to `main` |
| `:sha-abc1234` | one exact commit, immutable | every push to `main` |

Pre-releases (`v0.2.0-rc1`) get only their exact version tag — they never
move `:latest` or the floating minor tag.

Deployments should pin `:0.1.0` or `:sha-`; `:latest` is for people trying
WireMirage out. `docker-compose.yml`'s `wm-host` service (the `full` profile)
pulls the image, so a deployment never builds on the host:

```sh
WM_BOOTSTRAP_TOKEN=wmt_... WM_BOOTSTRAP_EMAIL=you@example.com \
  docker compose --profile full up -d
```

Override the tag with `WM_IMAGE` (e.g.
`WM_IMAGE=ghcr.io/einarfd/wiremirage:sha-abc1234`). To build the image from
source instead — to test a local change in the prod-shaped build — layer the
dev override:

```sh
docker compose -f docker-compose.yml -f docker-compose.dev.yml \
  --profile full up -d --build
```

The image binds `0.0.0.0:8080` inside the container. Publish it to loopback
only (`-p 127.0.0.1:8080:8080`) when a reverse proxy is in front.

## Probes

Two unauthenticated endpoints for orchestrators:

- `GET /health` — liveness, always 200 while the process is up.
- `GET /ready` — readiness; checks the configured backends and reports
  per-dependency status (e.g. `valkey: unreachable: ...`).

Neither is recorded in metrics or traces (high frequency, low value).

## Storage

`WM_STORAGE=redis://valkey:6379` is the deployment shape. Everything
WireMirage stores is **ephemeral by design** — routes and groups expire on
their TTL, journal entries default to 1 h, and state dies with its group.
There is nothing here to back up; users and tokens are the only durable
records, and they are cheap to recreate. Plan for the Valkey instance to be
disposable and you have the operational model right.

**Set `maxmemory` and `maxmemory-policy noeviction`**
([ADR-0005](adr/0005-valkey-storage.md)). The whole dataset lives in RAM, and
the sizing risk is journal volume rather than configuration: every request
that reaches a group's subdomain and matches nothing is recorded, with no rate
limit beyond a 4 KiB body cap and the 1 h TTL. A broken system under test — or
anyone who guesses a group name on a public host — can therefore push a lot of
short-lived data through.

`noeviction` is what keeps that a capacity problem instead of a correctness
one. Journal writes are best-effort and a rejected one is logged and skipped,
so the mock keeps answering; a store that evicted instead could drop a
`group:` record while its routes survived, and those routes have no TTL of
their own — the subdomain would start 404ing with the routes orphaned behind
it. Size for peak journal volume, not for the routes you expect to define.

## Production hardening

The defaults are tuned for plain-HTTP dev workflows. Before exposing the host
even on a trusted network behind a TLS edge, set the one behind-a-proxy
switch:

- **`WM_TRUSTED_PROXY=<hostname>`** (comma-separated for several) — turns on
  `Secure` cookies, `X-Forwarded-*` trust, and the MCP `Host` allowlist
  together ([ADR-0027](adr/0027-single-trusted-proxy-switch.md)). It's one
  setting so the posture can't be half-configured.

Then the first-deploy checklist:

- **Generate a strong `WM_BOOTSTRAP_TOKEN`** (`openssl rand -hex 32`) with
  `WM_BOOTSTRAP_EMAIL` set to your own email, so a later browser login reaches
  the same account. After the first deploy, mint an operator token
  (`wm tokens create operator/default`), revoke the bootstrap token
  (`wm tokens revoke bootstrap`), and unset both env vars — leaving them set
  re-creates the account on the next restart.
- **Generate a strong `SESSION_SECRET`** of at least 32 bytes (`openssl rand
  -base64 48`). Rotating it invalidates every existing session by design.
- **Bind the host to `127.0.0.1`** so the proxy is the only ingress.
  Combined with `WM_TRUSTED_PROXY`, the login throttle keys to the
  proxy-reported client IP and isn't spoofable. A directly reachable host with
  forwarded-header trust on can be hit with a spoofed `X-Forwarded-For`.
- **At the TLS edge**: turn on HSTS, set `X-Content-Type-Options: nosniff`,
  and consider a strict CSP — the UI only loads same-origin scripts (Ace is
  vendored under `/ui/static/ace/`).
- **Decide on egress.** `WM_EGRESS` is off by default; turn it on only if you
  want handlers to be able to call back into your systems, and keep the
  special-use default-deny in place (see
  [configuration](configuration.md#outbound-callbacks--egress)).
- **Think about who can log in.** Mock traffic is unauthenticated by design,
  so anything registered on this instance is world-reachable at its subdomain.
  Don't mock anything whose *responses* are sensitive, and don't put secrets
  in route paths.

## Scaling

Multiple replicas are supported. [ADR-0037](adr/0037-multi-replica-readiness.md)
audited the host for per-process state and closed every item:

- **The MCP transport is stateless**, so consecutive MCP requests may be served
  by different replicas.
- **The route table revalidates on a miss.** A route created on one replica is
  reachable from another without waiting for a restart: a match miss for a
  group that exists reloads that group from Valkey and retries once, at most
  once per group per 5s.
- **Route deletes and updates invalidate across replicas.** Mutations publish
  on a Valkey pub/sub channel; every replica subscribes and drops the affected
  records and compiled artifacts. This is what the read-through can't do on its
  own — a stale route still matches, so it never reaches the miss path. A lost
  message degrades to the read-through's staleness window rather than lasting
  until a restart.
- **Live journal tails span replicas.** A tail sees traffic dispatched by any
  replica, not just the one holding the connection. Its own replica's traffic
  is delivered locally and immediately, so a tail is armed the moment it
  opens; sibling traffic arrives over pub/sub, and replicas subscribe only
  while something is actually tailing them, so this costs nothing when nobody
  is watching.

  Cross-replica tailing needs **SUBSCRIBE**, not just PUBLISH. On a Valkey
  where publishing works but subscribing is blocked — restrictive ACLs, a
  proxy without pub/sub support — a tail still sees its own replica's traffic
  and silently misses everyone else's.
- **The login throttle is shared.** Five failed password attempts in a minute
  lock an IP out across every replica, not five per replica.
- **One replica sweeps per tick.** The lifecycle sweeper claims a short lease
  before each pass; the others skip it. Sweeping is idempotent, so this saves
  duplicated work rather than fixing a correctness problem — and if the holder
  dies mid-sweep the lease expires and the next tick proceeds.

One caveat remains, and it predates this work: an outbound callback
([ADR-0034](adr/0034-outbound-callbacks.md)) fires on the replica that served
the request, so a replica terminating during the delay drops that callback.
That sits inside the existing single-attempt best-effort contract, but rolling
deploys make it likelier.

### Kubernetes

A Helm chart lives at [`deploy/helm/wiremirage`](../deploy/helm/wiremirage/),
with its own [README](../deploy/helm/wiremirage/README.md) covering the three
prerequisites it can't provide for you: wildcard DNS for `*.{apex}`, a wildcard
TLS certificate (cert-manager with a **DNS-01** solver — HTTP-01 can't issue
one), and a Valkey instance.

Two things about the chart are worth knowing before you read it. It refuses to
render with in-memory storage, because a multi-replica release over it would
look healthy while each pod served a divergent view. And it gates liveness
behind a generous `startupProbe`: the host compiles the ~12 MB JavaScript
engine before binding its listener, and that artifact is cached in the
container's temp directory, so every fresh pod pays it once.

All of this requires the Valkey backend — `WM_STORAGE=memory` is a
single-process mode by construction, and every cross-replica mechanism above
short-circuits on it.
