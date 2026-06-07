---
name: wiremirage-debug
description: Use this skill when a WireMirage mock isn't behaving as expected — the SUT gets 404 instead of the mocked response, an existing route stopped firing, the handler returned a 500, the journal shows requests landing on the wrong route, or state isn't persisting the way you expect. This skill teaches the diagnostic loop using the journal, group inspection, and dry-run primitives. Reach for it when the main `wiremirage` skill's happy path isn't working.
---

# WireMirage debugging skill

This skill walks through the most common failure modes when a mock isn't doing what you expect. Each section is keyed off the symptom; pick the one that matches what you're seeing.

The first move in almost every case is to check the journal — it tells you whether the SUT's request even reached WireMirage and what the host did with it.

## "My SUT gets 404 from the mock"

Two cases: the request reached the host but didn't match a route, or it never reached the host at all.

```sh
# What would the host actually do with this request, in this group?
wm match -g stripe-mock POST /v1/charges
```

`wm match` is the fastest way to narrow this down. Matching is per-group
(each group is its own subdomain `{group}.{host}` and its own path space),
so the probe takes a `--group`/`-g`. If it prints `matched stripe-mock/3
(POST /v1/charges)` then the routing is fine and the question is whether the
request actually reaches the host. If it prints `no match. near-misses: ...`
the response tells you why — the most common shapes:

- `method_mismatch` — the path is right but you're sending GET when the route expects POST (or vice versa). Check the SUT.
- `prefix_match` — there's a route at `/v1/charges` and you're hitting `/v1/charge`, or you have a typo in either direction. Fix the path.

If `wm match` shows no near-misses, the request hasn't been mocked at all and the SUT is hitting an unmocked endpoint.

To confirm whether the request actually reached WireMirage:

```sh
# Did anything land in the journal recently?
wm journal list <group> --limit 5
```

For unmatched-traffic inspection, the unmatched log shows the unmatched
requests for groups you own (an admin sees every group's):

```sh
wm unmatched list                           # your groups', most recent first
wm unmatched list --path-pattern '/v1/*'    # narrow by path
wm unmatched show <n>                       # full record: headers, body, near-misses
```

Each entry has the request method, path, headers, body, and — if any route was close — a `near_misses` list explaining which routes almost matched and why (method mismatch vs. literal-prefix typo). Enough to see whether the SUT is calling the right host and what would have caught it.

## "My route exists but doesn't fire"

Two common causes: the group expired, or your method/path pattern is wrong.
(Under virtual-host routing each group is an isolated path space, so a route
in *another* group can never win this group's traffic — that whole class of
cross-group collision is gone.)

```sh
# What does the host actually match for the request the SUT is making,
# in this group?
wm match -g <group> <method> <path>

# Check the group's expiry. If it's gone, the routes went with it.
wm groups show <group>

# If the group is alive, list its routes and confirm yours is there.
wm routes list --group <group>

# Read the route's method/path verbatim and compare to what the SUT
# is actually sending. The journal entry's `request` field has the
# truth.
wm routes show <group>/<n>
```

`wm match -g <group>` is especially useful here: it probes only that group's
routes, so the slug in a `matched …` result confirms exactly which of your
routes wins. Within a group, path-pattern conflicts surface as 409 errors at
create time; a recently-deleted-and-recreated route can briefly leave the
prior route holding the path until cleanup completes.

## "My handler returned a 500 / the response shape is wrong"

Pull the journal entry — it has the structured error and any handler logs:

```sh
wm journal list <group> --limit 5
wm journal show <group>/<n>
```

The `error` field is non-empty when the handler trapped or threw; it includes the trap reason. The `handler_logs` array has anything the handler emitted via the host's `log` interface up to the point of failure. The `response` field shows what the host actually sent — useful when the handler succeeded but the headers/body don't match what the SUT expected.

For dry-run-shaped debugging without involving the SUT or polluting the journal, `wm routes test <group>/<n>` invokes the handler against a synthetic request. State reads see a point-in-time snapshot; state writes land in the snapshot and are discarded after the call:

```sh
wm routes test stripe-mock/1 --method POST --body '{"x":1}'
wm routes test stripe-mock/1 --kv counter=4   # seed state for this run
```

`wm match -g GROUP METHOD PATH` is the lighter-weight probe — it confirms the route would be *selected* without running the handler. Use it for "did my path pattern match" questions; use `routes test` for "did my handler produce the right response."

## "State isn't persisting between requests"

The handler has two stores: the per-route store (scoped to the route alone) and the per-group store (shared across the group's routes). They behave the same way; the question is usually which one you wrote to vs which one you're reading from.

```sh
# `wm groups state` lists what's actually persisted in either store.
wm groups state <group>             # list keys + value kinds
wm groups state <group> --snapshot  # full values (the listing truncates)
wm groups state <group> --clear     # nuke everything in both stores

# `wm routes state` is the per-route counterpart.
wm routes state <group>/<n>             # list this route's private kv
wm routes state <group>/<n> --snapshot  # full values for this route
wm routes state <group>/<n> --clear     # wipe this route only

# If state still seems wrong after inspection, write a probe handler
# that does `routeStore.set("probe", ...)` then `routeStore.get("probe")`
# in the same request and assert the round-trip — `wm routes test` is
# the easiest way to drive it without involving the SUT.
```

Common pitfall: each invocation gets a fresh wasmtime instance — don't expect global JS variables to persist. Anything you want to remember has to go through `routeStore` or `groupStore`. The ULID counters and rate-limit windows in handler examples persist this way.

## "I cleared state but the journal still shows old entries"

State and journal are separate. `wm groups state <group> --clear` wipes the kv stores; `wm groups journal <group> --clear` wipes the journal entries; deleting the group cascades both. The TTL on journal entries is 1h by default — entries older than that age out on their own.

## When these patterns don't surface the problem

If the symptoms don't fit any of the above, two things to try:

1. **Check the host's operational logs.** Bigger problems (Valkey unreachable, the embedded `js-engine.wasm` failing to instantiate, OTel pipeline broken) show up there before they show up in any of the user-facing surfaces. Ask the operator running WireMirage to share the relevant host logs.

2. **Check `/__health` and `/__ready`.** The readiness probe reports per-dependency status. If `valkey: unreachable: ...` shows up there, no amount of journal inspection will help — the host can't write to its backing store.

```sh
curl $WM_HOST/__health
curl $WM_HOST/__ready
```

If neither of those is the issue and the diagnostic patterns above don't help, the failure mode is novel and worth surfacing to the WireMirage team — please file an issue with the relevant journal entries (sanitized) and the symptoms.
