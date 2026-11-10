# ADR-0042: Query parameter names keep the case sent

**Status:** Accepted

Snapshot of the Arkiv `wiremirage` workspace ADR of the same name — edit
that one, then re-export; never edit this file in place.

## Context

The host parsed the request query string into `request.query` with names
lowercased (commit 61d5495, "to match the WIT contract"), while
`req.queryParam(name)` — and the `s3-slowdown` conformance lane's
`match.query` rules — matched names exactly. The two halves disagreed about
case: a request to `GET /claims/metering-points?customerId=C-1&brandCode=FK`
reached the handler as `[["customerid","C-1"],["brandcode","FK"]]`, and
`req.queryParam("customerId")` returned `undefined` with no error, so a
handler written from the real API's own docs looked as if the client had
never sent the parameter. That cost real debugging time (field report
2026-11-10): the first suspect was the client, not the mock. Lowercasing is
correct for header names (case-insensitive per RFC 9110) and wrong for
query names, which are case-sensitive per RFC 3986 — `customerId` and
`customerid` are different parameters, and camelCase query names are common
in real APIs.

## Decision

Query parameter names are preserved exactly as sent. `request.query`
carries as-sent names; `req.queryParam(name)` matches exactly
(case-sensitive), first match wins for repeated names. Values stay
form-decoded (`+` → space, then percent-decode). Header handling is
unchanged: names lowercased, `req.header(name)` case-insensitive. The
journal records the same as-sent pairs the handler saw. This is a breaking
change to the handler contract, allowed pre-1.0.

## Consequences

- Handlers written against the lowercased contract (e.g.
  `req.queryParam("customerid")`) stop matching camelCase clients; they must
  use the case the client actually sends. The contract docs are updated in
  the same change: `wit/wiremirage.wit`, ../script-api-wit.md,
  `types/wiremirage-handler.d.ts`, the `get_capabilities` `request` topic,
  `docs/handlers.md`, and the CHANGELOG.
- The `s3-slowdown` conformance lane's exact-match `query` rules now work
  for any case.
- Route matching is unaffected — path patterns never consult the query.
- No stored data depends on the old behavior: journal `query` was always
  empty until the companion journal fix landed, and dry-run passes
  caller-supplied pairs through verbatim.

## Alternatives considered

- Keep lowercasing and make `queryParam` case-insensitive: rejected — `Foo`
  and `foo` are distinct parameters on the wire; a case-insensitive lookup
  over a lowercased list cannot tell them apart and first-match wins
  arbitrarily. It would also keep the silent-`undefined` trap for handlers
  that filter `req.query` by exact name.
- Lowercase only for matching, preserve in the array: rejected — two
  sources of truth; `req.query` and `queryParam` could disagree, which is
  exactly the class of bug this ADR removes.

## See also

- ../script-api-wit.md — the WIT contract this amends
- [0003-component-model.md](0003-component-model.md) — WIT as the handler contract
