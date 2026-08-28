# WireMirage Helm chart

Deploys the WireMirage host on Kubernetes. One Deployment, one Service,
one Ingress, and a ConfigMap; storage and secrets come from outside the
release.

```sh
helm install wm ./deploy/helm/wiremirage \
  --set apexHost=wm.example.com \
  --set existingSecret=wiremirage-secrets \
  --set valkey.url=redis://valkey.default.svc:6379 \
  --set ingress.tls.secretName=wm-wildcard-tls
```

## Three prerequisites

The chart refuses to render without the first two and cannot check the
third, so read these before installing.

### 1. Wildcard DNS

Every group is served on its own subdomain — `{group}.{apexHost}`
([ADR-0030](../../../docs/adr/0030-virtual-host-routing.md)) — and groups
are created at runtime by agents. A DNS record per group is therefore
not an option: `*.{apexHost}` must resolve to your ingress, alongside
the apex itself.

Without it, mock requests fail at the ingress and never reach a pod. The
symptom is a 404 from the ingress controller rather than from
WireMirage, which is worth knowing when debugging.

### 2. A wildcard TLS certificate

For the same reason, TLS must cover `*.{apexHost}`. **The chart does not
issue it.** Name an existing Secret in `ingress.tls.secretName`.

HTTP-01 cannot satisfy a wildcard, so on Kubernetes this is normally
cert-manager with a **DNS-01** solver against your DNS provider. If you
already terminate TLS upstream of this ingress, set
`ingress.tls.enabled=false` instead.

### 3. Valkey

`valkey.url` is required; the chart deploys no storage. Valkey is real
infrastructure with its own sizing, persistence and failover story, and
bundling a cache into an app chart mostly serves to get an unbacked one
into production.

More than one replica *requires* it. Every cross-replica mechanism —
route-cache invalidation, journal fan-out for live tails, the shared
login throttle, the sweep lease
([ADR-0037](../../../docs/adr/0037-multi-replica-readiness.md)) — goes
through Valkey and short-circuits on in-memory storage. The chart cannot
express `WM_STORAGE=memory` at all, deliberately: a multi-replica
release over in-memory storage would look healthy while each pod served
a private, divergent view of the world.

If the URL carries a password, put the whole URL in your Secret and set
`valkey.urlSecretKey` to its key instead of `valkey.url`.

## Secrets

`existingSecret` names a Secret you create out of band. The chart never
templates secret values from `values.yaml` — they would otherwise land
in `helm get values`, in shell history, and in any values file you
commit.

| Key | When |
|---|---|
| `SESSION_SECRET` | Always. At least 32 bytes; `openssl rand -base64 48`. Rotating it signs everyone out. |
| `WM_BOOTSTRAP_TOKEN` | First boot, paired with `bootstrapEmail`. Ignored once any user exists. |
| `WM_OIDC_CLIENT_SECRET` | `auth.oidc.enabled` |
| `WM_GITHUB_CLIENT_SECRET` | `auth.github.enabled` |
| `WM_STORAGE` | `valkey.urlSecretKey` is set |

The whole Secret is mounted with `envFrom`, so any additional key in it
reaches the host as an environment variable.

## Authentication

Pick whichever browser login you want, or none. The chart enables no
provider by default, and that is a valid deployment: the host only
refuses to start when there are no users *and* no login method at all,
and a `WM_BOOTSTRAP_TOKEN` counts as one. So a first install with just
`bootstrapEmail` and that token in your Secret comes up fine, and you
add a provider later.

- **GitHub** — set `auth.github.enabled`, `clientId`, and at least one
  of `allowUsers` / `allowOrgs`. The allow rule is required; without one
  any GitHub account could sign in, so the chart refuses to render.
- **OIDC** — any compliant issuer (Pocket ID, Keycloak, Authentik,
  Okta). See below; it needs an allow posture too.
- **Both** — they coexist; the login page offers each configured one.
- **Neither** — fine, as above. API tokens still work.

### OIDC allow posture

The host requires **exactly one** posture and refuses to start without
it, so the chart refuses to render without it — a crash loop is a worse
way to learn this than a template error.

| Value | Meaning |
|---|---|
| `auth.oidc.allowAll` | Every user the issuer authenticates. Right for a *private* IdP (Pocket ID, closed-registration Keycloak, corporate Okta), where account existence already is the authorization decision. **Never** against a public issuer like Google, where "authenticated" means anyone on the internet. |
| `auth.oidc.allowEmails` | Exact addresses. |
| `auth.oidc.allowDomains` | Domain, not address: `acme.example` allows anyone@acme.example. |
| `auth.oidc.allowGroups` | Matched against the IdP's groups claim; needs `auth.oidc.groupsClaim`. |

The per-identity rules OR together. Setting them *and* `allowAll` is
refused, by the host and by the chart: the combination usually means
someone believes the rules still restrict something they don't.

`auth.oidc.adminEmails` and `adminGroups` promote matching users on
first login; they are not allow rules and do not satisfy the posture
requirement.

## Startup and probes

The host compiles the ~12 MB JavaScript engine component before it binds
its listener, so a fresh pod is unreachable until that finishes — about
**2 seconds** on the release image, and every pod pays it, since the
compiled artifact is cached under `/tmp` and does not survive a pod.

That is what the `startupProbe` is for: it holds off liveness during
those two seconds rather than tolerating anything lengthy. The default
60s of grace is headroom for a slow node; raise
`probes.startup.failureThreshold` if you have one.

Liveness probes `/health`, not `/ready`. `/ready` pings Valkey, and a
storage blip should take pods out of rotation, not restart them.

The probes work with the bare `Host: <podIP>:8080` header the kubelet
sends. Direct-IP access falls through to the control plane, so only a
recognised `{group}.{apex}` subdomain is treated as mock traffic and
`/health` cannot be shadowed by a tenant's route.

## Portability

The manifests use only current, stable APIs — `apps/v1`,
`networking.k8s.io/v1` (Ingress, stable since 1.19), `policy/v1` (PDB,
stable since 1.21) and core `v1`. Nothing beta, nothing deprecated. It
should install on any conformant cluster from 1.21 onward.

Pods satisfy the **restricted** Pod Security Standard: non-root with an
explicit uid, no privilege escalation, all capabilities dropped, a
read-only root filesystem, and the default seccomp profile. That matters
on clusters that enforce PSS by namespace label, which managed offerings
increasingly do by default.

Two things to check for *your* cluster, because they are the parts a
chart cannot settle:

- **Your ingress controller must handle a wildcard host rule.** The
  chart emits a `*.{apexHost}` rule because groups get subdomains at
  runtime. `ingress.className` is unset by default, which means your
  cluster's default IngressClass — and on some managed clusters that
  default is a cloud load-balancer controller with its own certificate
  model rather than one that reads a TLS Secret. If yours is like that,
  either set `ingress.className` to an nginx-style controller or adapt
  the ingress template to whatever your controller expects. This is the
  most likely thing to need adjusting.
- **Your Valkey/Redis must allow pub/sub.** `PUBLISH` *and* `SUBSCRIBE`,
  not just the former — cross-replica route invalidation and live
  journal tailing both depend on it. Managed cache offerings sometimes
  restrict pub/sub or behave differently in cluster mode. A single
  replica works either way; a tail silently misses sibling traffic if
  SUBSCRIBE is unavailable.

For a multi-zone cluster, set `topologySpreadConstraints` — the chart
leaves them empty, so nothing stops a scheduler putting every replica in
one zone.

## What this chart does not do

- **Issue certificates.** See prerequisite 2.
- **Run Valkey.** See prerequisite 3.
- **Autoscale.** Add an HPA if you want one; the host is stateless
  enough for it, and each new pod costs about two seconds of engine
  compile before it serves.
- **Survive a callback mid-flight.** An outbound callback
  ([ADR-0034](../../../docs/adr/0034-outbound-callbacks.md)) fires on the
  replica that served the request, so a rolling deploy can drop one that
  is waiting out its delay. That sits inside the existing single-attempt
  best-effort contract, but rolling deploys make it likelier than a
  single host did.

## Values

See [values.yaml](values.yaml); every field is commented. The ones with
no safe default — `apexHost`, `existingSecret`, `valkey.url`,
`ingress.tls.secretName` — fail at render time with a message explaining
what to set, rather than deploying a host that would fail at boot.
