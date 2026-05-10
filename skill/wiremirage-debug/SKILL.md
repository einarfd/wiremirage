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
# Did anything land in the journal recently?
wm journal list <group> --limit 5

# Or check across all of your groups (admin) — the most recent
# entries usually point at where the issue is.
```

If you see the request in the journal of *some* group, the route is matching the wrong thing. If you don't see it anywhere, either the request didn't reach WireMirage (network / DNS / wrong host URL on the SUT side) or it landed without matching any route.

For the unmatched case, check the host's unmatched log via the REST API directly — it's admin-only:

```sh
curl -H "Authorization: Bearer $WM_TOKEN" $WM_HOST/__api/unmatched | jq .
```

Each entry has the request method, path, headers, and body — enough to see why nothing matched (typo in the path, wrong method, the SUT is calling a different host).

A `wm match METHOD PATH` probe and a `wm unmatched list` CLI command are both planned; they aren't shipped yet. Use the REST endpoint above for now.

## "My route exists but doesn't fire"

Three common causes: the group expired, another route in another group is winning, or your method/path pattern is wrong.

```sh
# Check the group's expiry. If it's gone, the routes went with it.
wm groups show <group>

# If the group is alive, list its routes and confirm yours is there.
wm routes list

# Read the route's method/path verbatim and compare to what the SUT
# is actually sending. The journal entry's `request` field has the
# truth.
wm routes show <group>/<n>
```

Path-pattern conflicts surface as 409 errors at create time, but two routes in *different* groups can both technically claim the same path; the host serializes — only one wins. If you've recreated the group recently, the older route may still be holding the path.

## "My handler returned a 500 / the response shape is wrong"

Pull the journal entry — it has the structured error and any handler logs:

```sh
wm journal list <group> --limit 5
wm journal show <group>/<n>
```

The `error` field is non-empty when the handler trapped or threw; it includes the trap reason. The `handler_logs` array has anything the handler emitted via the host's `log` interface up to the point of failure. The `response` field shows what the host actually sent — useful when the handler succeeded but the headers/body don't match what the SUT expected.

A `wm routes test <slug>` dry-run that invokes the handler against a synthetic request without journaling is planned; not shipped yet. For now, send the request through the live mock and read the journal.

## "State isn't persisting between requests"

The handler has two stores: the per-route store (scoped to the route alone) and the per-group store (shared across the group's routes). They behave the same way; the question is usually which one you wrote to vs which one you're reading from.

```sh
# `wm groups state` lists what's actually persisted in either store.
# (Slice 11 only shows clear; a read variant is planned.)
wm groups state <group>          # list keys (when supported)
wm groups state <group> --clear  # nuke everything in both stores

# In the meantime, the simplest probe is to write a value and then
# read it back from the same handler in a follow-up request, both via
# the same `routeStore.set(...)` / `routeStore.get(...)` interface.
```

Common pitfall: each invocation gets a fresh wasmtime instance — don't expect global JS variables to persist. Anything you want to remember has to go through `routeStore` or `groupStore`. The ULID counters and rate-limit windows in handler examples persist this way.

## "I cleared state but the journal still shows old entries"

State and journal are separate. `wm groups state <group> --clear` wipes the kv stores; `wm groups journal <group> --clear` wipes the journal entries; deleting the group cascades both. The TTL on journal entries is 1h by default — entries older than that age out on their own.

## When these patterns don't surface the problem

If the symptoms don't fit any of the above, two things to try:

1. **Check the host's operational logs.** Bigger problems (Valkey unreachable, compiler sidecar down, OTel pipeline broken) show up there before they show up in any of the user-facing surfaces. Ask the operator running WireMirage to share the relevant host logs.

2. **Check `/__health` and `/__ready`.** The readiness probe reports per-dependency status. If `valkey: unreachable: ...` shows up there, no amount of journal inspection will help — the host can't write to its backing store.

```sh
curl $WM_HOST/__health
curl $WM_HOST/__ready
```

If neither of those is the issue and the diagnostic patterns above don't help, the failure mode is novel and worth surfacing to the WireMirage team — please file an issue with the relevant journal entries (sanitized) and the symptoms.
