# ADR-0034: Outbound callbacks (webhooks) — host-orchestrated, deployment-gated egress

**Status:** Accepted

**Context:**

WireMirage handlers have **no network egress**. The handler WIT world imports
only `store` / `log` / `clock` / `response-stream` (../script-api-wit.md),
and the shared JS engine's web globals (`fetch`, `WebSocket`, …) throw a
catchable error rather than reaching the network (the 2026-06-09 MCP
field-report fix). The closed sandbox is deliberate ([0002-wasm-sandbox.md](0002-wasm-sandbox.md)).

The one capability this forecloses that has a real, frequent use case is the
**async webhook**: a mock receives a request (create a charge), responds, and
*then* POSTs a callback to the system-under-test's (SUT's) webhook URL —
Stripe / GitHub / payment-confirmation style. That is the only way to exercise
the SUT's webhook-*receiver* code path against a mock. The status-quo workaround
is to have the **test harness** post the webhook itself; it works but is less
faithful (the test, not the mock, plays the service, so the send isn't part of
the mock's modeled behaviour).

Adding egress is the first network egress out of the sandbox, so it needs a
deliberate policy. An initial "multi-tenant SSRF, off because we can't trust
tenant code" framing was rejected as too paranoid: this deployment has named,
invited user accounts, not anonymous public signup — egress scope is an
**operator policy decision**, not an anti-adversary default. The residual
concern that survives even for trusted users is **accidents** (a buggy handler
hitting the cloud metadata endpoint and leaking instance creds), which calls for
a thin guardrail, not a lockdown.

**Decision:**

Add outbound callbacks with the following shape.

- **Host-orchestrated deferred callback, not in-handler `fetch`.** A new host
  import `host.scheduleCallback({ url, method, headers, body, delayMs })` on the
  handler contract. The handler computes the full callback request at
  request-time and hands it to the host; the **host** fires it on a background
  task after `delayMs`, *after* the original response is sent. Keeping egress in
  the host (not a general in-handler HTTP client) centralises the egress policy
  + journaling in one place, matches the async timing of webhooks (the callback
  is after the response, not during), and avoids handing handlers a blocking
  general-purpose client. Imperative host import (consistent with `host.sleep` /
  `host.responseStream`), so it works for buffered *and* streaming handlers.

- **Single-attempt, best-effort, journaled delivery.** Fire once, with a
  per-callback timeout and bounded concurrency. The outcome (status / error) is
  recorded in the journal as a callback entry — it can't surface on the original
  response (already returned), so the agent/test inspects it there. **No
  retries, no durable queue:** if the host restarts or the group TTLs away
  before a delayed callback fires, it is dropped. This matches the ephemeral /
  no-backup posture ([0008-handlers-in-storage.md](0008-handlers-in-storage.md)); a down SUT endpoint means
  the test is broken regardless, so retry buys nothing. **Single-attempt best-effort is a policy choice, not a
  deferral:** WireMirage is a mock fixture, not a webhook delivery system, so
  retry / backoff / durable-queue machinery is a deliberate non-goal — not a
  roadmap item.

- **Egress policy: deployment-gated operator knob + accident guardrail.**
  Mirrors the deployment-shape config pattern of `WM_LOCAL_AUTH` /
  `WM_TRUSTED_PROXY` ([0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md)).
  - Egress is **off unless the operator enables it.** When on, it targets the
    public internet; a **hardcoded default-deny of special-use ranges** always
    applies as the accident guardrail: loopback (`127.0.0.0/8`, `::1`),
    link-local incl. the `169.254.169.254` cloud-metadata IP (`169.254.0.0/16`,
    `fe80::/10`), private (`10/8`, `172.16/12`, `192.168/16`), CGNAT
    (`100.64/10`), ULA (`fc00::/7`), and multicast / reserved. (The metadata IP
    is a generic cloud convention — AWS / GCP / Azure / Hetzner — not
    deployment-specific.)
  - `WM_EGRESS_ALLOW=<cidr/host,…>` (IPv4 **and** IPv6 CIDRs) **overrides** the
    default-deny — the legitimate self-hosted / CI need is to *allow* an internal
    range where the SUT lives, not to add more blocks. An optional
    `WM_EGRESS_DENY` covers stricter operators who want extra public ranges
    blocked. (A pure block-list with no allow-override was rejected — see
    alternatives.)
  - **Enforcement is on the resolved IP, not the URL string.** Resolve the
    hostname, check *every* resolved address against the rules, normalise
    IPv4-mapped IPv6 (`::ffff:a.b.c.d`) before checking, and disallow or re-check
    redirects. A string-based filter is trivially bypassed (DNS rebinding, a
    hostname resolving to a blocked IP, v6-mapping) — this check is the
    security-critical part of the slice.

- **Per-group opt-in.** A `callout_enabled` flag on the **group** (the tenancy
  boundary, [0030-virtual-host-routing.md](0030-virtual-host-routing.md)). The host config enables the
  *capability*; a group opts *in* to using it. So turning egress on host-wide
  does not auto-grant it to every group — each tenant enables it for their own
  group, it's auditable, and the blast radius is scoped to that group.
  `scheduleCallback` from a group without the flag is rejected with a clear
  error. Rides the existing group-config surface (alongside TTL / sliding)
  rather than a per-route field.

The WIT contract change (the `scheduleCallback` import) updates
../script-api-wit.md first, then `wit/engine.wit` (the shared-engine world), per the contract-mirroring rule. Like `host.responseStream` (ADR-0022), the `scheduleCallback` import is **engine-internal** — it lands on the `engine` world, NOT the user-facing `world handler` in `script-api-wit.md`. The public handler input is source-language only (ADR-0023), so only the shared JS engine needs it; putting it on `world handler` would force every pre-compiled-wasm fixture to relink against an import none use, for no gain.

**Consequences:**

- Webhook-*receiver* testing becomes possible against the mock: the mock plays
  the real service end to end, instead of the test stubbing the send.
- First network egress from the sandbox — a genuine security surface, mitigated
  in depth: off-by-default, operator allow-list, hardcoded special-use deny,
  resolved-IP enforcement, per-group opt-in, single-attempt (no amplification),
  bounded concurrency and per-callback timeout.
- **Cost:** a WIT import + bindings; a host background-fire task + a callback
  journal entry type; the egress resolver/filter (the security-sensitive piece —
  the resolved-IP check must be correct); the `WM_EGRESS_*` config; and the group
  `callout_enabled` field threaded through storage + REST / MCP / CLI / UI.
- **Ephemeral / best-effort:** callbacks are lost on host restart or group
  expiry; documented, no delivery guarantees.
- Primarily a **self-hosted / trusted-deployment** feature; the shared host
  enables it at the operator's discretion via the allow-list.
- **Deferred (genuinely open — could land if a use case appears):** an in-handler
  synchronous `fetch` egress client; and callback payloads computed by a *second*
  handler invocation at fire time (the handler computes the payload up front for
  now). Note retry / durable delivery is deliberately *not* in this list — it's a
  non-goal (see the Decision), not a deferral.

**Alternatives considered:**

- **In-handler synchronous `fetch` / HTTP client.** Rejected as the primary
  shape: it blocks the handler, has the wrong timing for webhooks (egress is
  after the response, not during), and hands handlers a general egress client
  (larger surface) rather than a declarative, host-controlled callback the host
  can police and journal. Could be added later if a genuine synchronous-call use
  case appears.
- **At-least-once / durable delivery with retries + backoff.** Rejected for v1:
  that is mocking a real webhook *sender*'s delivery semantics, which a test
  fixture doesn't need — single-attempt + an observable outcome is enough, and a
  down SUT means the test is already broken. Chasing it opens the can of worms
  (backoff schedule, a pending-queue surviving restarts, dead-letter) for no test
  value. So this is a deliberate non-goal: single-attempt best-effort is the
  design, not a stepping stone toward delivery guarantees.
- **Test harness posts the webhook itself (no egress).** The status-quo
  workaround — works and needs no sandbox hole, but the test, not the mock,
  plays the service, so the mock isn't faithful and the send isn't modelled
  behaviour. In-mock egress is strictly more faithful; we accept the cost.
- **Declarative callbacks in the response value** (`response.callbacks: [...]`)
  instead of an imperative import. Cleaner for buffered handlers, but streaming
  handlers return no response value, so it wouldn't be a single mechanism; the
  imperative `host.scheduleCallback` matches the existing `host.*` import style
  and covers both.
- **Pure block-list env var, no allow-override.** Rejected: it re-types the
  well-known special-use ranges every operator already wants blocked, and
  doesn't serve the real need (allowing an internal CI range past the default
  deny). The allow-override on a hardcoded default-deny is the useful shape.
- **Open egress to every route when host-enabled.** Rejected: no least-privilege
  — enabling egress would hand it to every group's handlers on a shared host.
- **Per-route opt-in.** Rejected as too fine-grained: a new field across every
  surface for a capability most routes never use. Per-group rides the tenancy
  boundary and the existing group-config surface.

**See also:** [0002-wasm-sandbox.md](0002-wasm-sandbox.md) (the closed sandbox this opens),
[0030-virtual-host-routing.md](0030-virtual-host-routing.md) (groups as the tenancy boundary → per-group
opt-in), [0027-single-trusted-proxy-switch.md](0027-single-trusted-proxy-switch.md) (deployment-shape config
pattern), [0022-streaming-http-responses.md](0022-streaming-http-responses.md) (bounded-channel / background-task
machinery to reuse), [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) (`host.*` import
precedent), ../script-api-wit.md (the handler contract the import lands on),
../route-model.md
