# ADR-0039: Structural admin gating in the web UI — the path prefix carries the privilege

**Status:** Proposed

## Context

On 2026-09-03 a self-service action was found sitting inside an admin-gated URL subtree, where it had been since the Settings screen shipped on 2026-08-27.

"Sign out everywhere" bumps the caller's own session epoch. It can only ever affect the caller, so its handler was — correctly, and deliberately, with a doc comment saying so — not admin-gated. But it was registered at `POST /ui/settings/sessions/revoke-all` and its only affordance was a button on `/ui/settings`, which returns 403 to non-admins. The result: every non-admin on the host had no browser path to ending their own sessions. The remedy available to them was to hand-roll the REST call with an API token, which is no help when the credential they are worried about is a stolen browser session.

The action has been relocated to `/ui/me/sessions/revoke-all` and now renders on the tokens page. That fixes the instance. It does not touch the mechanism that allowed the misfiling, or that made it invisible in review.

### How admin decisions are expressed today

An audit of the two authenticated surfaces:

- **Web UI.** Every admin decision is a hand-written inline check in `ui/mod.rs`. Six are pure admin gates of the form `if !auth.is_admin { return forbidden_page(...) }` — the settings page, its three mutating POST actions, and two list-scope helpers. There is no helper function and no middleware. The privilege is expressed only inside handler bodies.
- **REST.** There is a named helper, `require_admin(&auth)?`, used at four call sites. Other admin decisions are result scoping rather than gating (`if auth.is_admin` selects "everyone" instead of "mine").

A further eleven sites in `ui/mod.rs` take the form `if !auth.is_admin && resource.owner_id != auth.user_id`. These are owner-or-admin authorization, which is a different thing from an admin gate, and this ADR is careful not to conflate them.

### Why the inline form failed

Because the check lives inside the handler, nothing about the router says which routes are privileged. A reader auditing `ui::router()` sees a contiguous `/ui/settings/*` block and reasonably concludes the whole subtree is admin-only. It was not, and discovering that required opening five handler bodies.

That cuts in two directions and both are live:

- **False assurance.** Someone reviewing the router for privilege boundaries concludes the subtree is protected. One route in it is not.
- **Silent removal.** Someone hoisting five repeated `if !auth.is_admin` checks into one layer over `/ui/settings/*` — the obvious tidy-up, and the change this ADR would otherwise invite — deletes a security capability that users rely on. The test suite would not have stopped them: all five session tests posted directly to the URL, four of them with an admin cookie, and none loaded the page as a non-admin. The behaviour was well covered; the reachability was not covered at all. (A `sessions_card_is_reachable_by_non_admins` test now closes that specific hole.)

### Prior art on the prefix

`/ui/admin/*` is unused and uncontested. The one screen ever specified under it, `/ui/admin/health`, was decided **not planned** on 2026-08-27 and its placeholder route has already been deleted from the host. Four `/api/admin/*` endpoints were removed the same day. Neither decision rejected the prefix as a naming device; both rejected the specific screens for duplicating the OTLP surface.

## Decision

### 1. The invariant

**A path prefix must not imply a privilege it does not enforce.** Either every route under a prefix carries the privilege the prefix names, or the prefix does not name a privilege at all.

This is what the incident violated, and stating it settles cases that otherwise look identical:

- `/ui/settings/*` named a privilege — a screen only admins can open — and had one exception under it. Wrong under the invariant.
- `/api/users/*` names a **resource**, not a privilege. `/api/users/me` sitting beside admin-only siblings is correct and stays. Nothing about the prefix claims the contents are admin-only.

### 2. The UI's admin routes become a subtree behind a layer

`/ui/settings` and its three action routes move to `/ui/admin`, built by a dedicated `admin_router()` that applies a `require_admin` middleware via `middleware::from_fn_with_state` and is merged into the UI router. The layer renders the same styled 403 that `forbidden_page` produces today, so the response is unchanged. The inline `if !auth.is_admin` checks in the moved handlers are deleted — the layer subsumes them, and leaving both would restore the ambiguity about where the gate lives.

Layer ordering: the admin gate sits inside the existing session and CSRF layers. It needs an authenticated `AuthContext`, and a request that fails CSRF should be rejected as a CSRF failure rather than masked by a 403.

The nav label becomes **Admin**. "Settings" names a topic, not a privilege, and the vagueness is not harmless: it is part of why a user looking for account-level settings goes to the admin screen and finds a user table.

### 3. What deliberately does not change

- **The eleven owner-or-admin checks stay inline.** Their answer depends on the record being fetched, not on the path, so no router layer can express them. This ADR governs privilege that is constant across a subtree; per-resource authorization is [0011-route-ownership.md](0011-route-ownership.md)'s business and stays where it is.
- **REST keeps `require_admin` and gains no prefix.** Its admin endpoints are resource-shaped and not contiguous, it is a programmatic surface with CLI and agent clients for whom a URL move is a real break, and `/api/users/me` is a deliberate non-admin route inside an otherwise-admin resource — precisely the shape the invariant declares correct. REST's weakness was never the missing prefix; it already has a named helper whose absence is visible at review, which is the property the UI lacked entirely.

## Consequences

- For routes whose privilege is constant, the bug class becomes structurally impossible. Registering a route in the admin subtree gates it. The remaining way to get it wrong is to register an admin page *outside* the subtree, which the naming makes conspicuous in review rather than invisible.
- **Enforcement is by construction, not by test, and this is a real limitation.** axum exposes no route table, so no test can enumerate the mounted routes and assert that every `/ui/admin/*` path refuses a non-admin. The guarantee comes from the router's shape. A sweep test over a hand-maintained list of admin paths is added regardless — it would have caught this incident the day it landed — but it is a backstop, and it goes stale silently when someone adds a route and forgets the list.
- Four URLs move. All are an admin page or POST form targets, none is an API anyone scripts. Pre-1.0, so no redirect is left behind.
- The refactor that would have been unsafe becomes the safe one. Anyone consolidating the repeated checks into a layer now finds the layer already there and every route under it genuinely admin-only.
- One additional middleware in the UI router. The cost is a boolean read on an already-extracted context; not measurable against a request that renders a template.
- `web-ui-design.md` needs two corrections in the same change: the Settings screen re-titled to Admin with its new path, and the not-planned `/ui/admin/health` entry reconciled so a dead screen is not the only thing claiming a prefix that is now in use.

## Alternatives considered

- **Keep inline checks; add a review rule or lint requiring an `is_admin` check in every handler under an admin path.** Rejected, and instructively: the incident was not a missing check but a correctly absent one in a misleading place. This rule would have demanded the wrong fix — adding a gate to a self-service action — and would have made a non-admin's inability to sign out everywhere permanent and documented.
- **Keep `/ui/settings` and simply add the layer.** The cheapest option, and the gate would be structural. Rejected on the invariant's own terms: "settings" names a topic, so the prefix still would not tell a reader what it enforces, and the next settings-shaped self-service feature arrives at the same fork with the same wrong answer available. The rename is most of the value, not decoration on it.
- **A typed extractor — handlers take `AdminContext` instead of `AuthContext`, so a missing gate is a compile error.** Genuinely attractive and stronger than a layer for the handler half, since it cannot be forgotten. Rejected as the primary mechanism because it is per-handler again: it makes each handler's own requirement explicit but says nothing about the subtree, so it could not express "everything under this prefix is admin-only" and would not have flagged a self-service route filed among admin ones — the actual failure. Worth adopting later as a complement to the layer, not a replacement for it.
- **Move REST's admin endpoints under `/api/admin/*` for symmetry.** Rejected: it breaks every client including `wm users`, revives a prefix retired on 2026-08-27, and misreads `/api/users` as naming a privilege when it names a resource.
- **Do nothing; the bug is fixed.** Rejected. The fix relocated one route. The mechanism that let it be filed wrongly, and that kept the misfiling out of view in every subsequent review of that router, is untouched — and the tidy-up most likely to be attempted next on that code is the one that would reintroduce the harm in a worse form.

## See also

- ../web-ui-design.md — the Settings/Admin screen, and the Tokens screen that now owns sessions
- ../rest-api.md — `require_admin`, the resource-shaped admin endpoints, and the retired `/api/admin/*` block
- ../auth-and-authz.md — the session epoch behind "sign out everywhere"
- [0011-route-ownership.md](0011-route-ownership.md) — owner-or-admin authorization, the per-resource checks this ADR leaves inline on purpose
- [0009-html-htmx-ui.md](0009-html-htmx-ui.md) — the server-rendered UI this applies to
- [0018-local-user-accounts.md](0018-local-user-accounts.md), [0036-email-only-identity.md](0036-email-only-identity.md) — where the admin flag comes from
