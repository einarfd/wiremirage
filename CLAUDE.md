# CLAUDE.md

Bootstrap notes for Claude Code working in this repository.

## What this is

WireMirage: an agent-native, multi-language mock server. Handlers are real
code (TypeScript first), compiled to Wasm components, executed inside a Rust
host (`wasmtime`). Per-route isolated KV state; groups as TTL-bounded
lifecycle units. Storage in Valkey (Redis wire protocol). See `README.md`.

**Status:** slices 1–46 landed. The WIT contract is live at
`wit/wiremirage.wit`, the host (`wm-host`) instantiates components
against it, storage is abstracted behind a `Storage` enum with both
in-memory and Valkey backends, and routes are stored in a `Registry` +
`RouteTable` keyed by `{group}/{n}` slugs per `route-model.md`. The
REST API at `/__api/routes` supports POST/GET/PATCH/DELETE for
source-language handlers. The only public artifact input is `source`
+ `language` (`typescript` / `javascript`); pre-compiled wasm upload
was retired from the public surface in ADR-0023 (routes still run as
wasm internally, and the registry's internal `NewRoute` keeps the
`compiled_wasm` field for the shared engine, fixtures, and a future
AOT sidecar). Source-language
(JS / TS) compiles in-host (ADR-0020): a shared `js-engine.wasm` is
embedded into the host binary, TypeScript runs through pure-Rust swc
before storage, dispatch instantiates the shared engine per request
with the per-route source threaded through a host import. No Node
sidecar. Source handlers can also **stream** responses (ADR-0022):
`host.responseStream({status,headers})` → `.write(chunk)` / `.close()`
flushes chunks to the wire incrementally (chunked transfer-encoding)
for SSE / streaming-LLM / MCP-transport mocks. Engine-internal
`response-stream` WIT imports (`start`/`write-chunk`/`finish`) on the
`engine` world; the dispatch `select!`s head-vs-completion and pumps a
bounded channel with backpressure + client-disconnect signalling.
Streaming handlers run up to ~5 min (vs the ~30s buffered engine
epoch); journaled with a `[stream] N chunks, M bytes, <disposition>`
summary; dry-run collects the chunks in-process. Per-route AOT
components stay buffered (handler world unchanged). The
`/__api/*` surface is gated by bearer-token auth (bootstrap via
`WM_BOOTSTRAP_TOKEN=wmt_...` on first startup); mock traffic to user
routes stays open by design. Public probes: `GET /__health`,
`GET /__ready`. Token CRUD lives at `/__api/tokens`; user CRUD lives
at `/__api/users` (admin-only for cross-user actions, plus
`GET /__api/users/me` for any authed caller). Routes carry `owner_id`
and PATCH/DELETE check owner-or-admin. Every dispatched mock request
lands in a per-group journal (`/__api/journal/{group}`); unmatched
requests land in `/__api/unmatched` (admin-only). Both default to a
1h TTL. Groups are first-class lifecycle units (`/__api/groups`) with
configured TTL (default 24h, max 30d) and sliding-on-traffic by
default; cascade-delete wipes routes, kv/gkv state, and journal
entries together. A background sweeper reaps the children of any
group whose Valkey TTL has fired. The `wm` CLI (slice 9) wraps the
REST surface end-to-end: groups, routes, journal, tokens, plus the
public probes. Auth via `WM_TOKEN` / `--token`, host via `WM_HOST` /
`--host`. `--json` switches to machine-parseable output for scripts
and agents. The MCP server (slice 10) is part of `wm-host` and
mounts at `/__api/mcp` over the streamable-HTTP transport (rmcp).
20 tools now cover identity, discovery, group/route CRUD (with
slice-15 `update_route`), group state, the slice-11 streaming pair
(`wait_for_request`, `tail_journal`) backed by `GET
/__api/journal/tail` SSE on the host and a single-host broadcast bus
inside `Journal`, the slice-13 match probe (`find_route` MCP tool +
`wm match` CLI + `GET /__api/match` host endpoint with
`method_mismatch` and `prefix_match` near-misses), and the slice-16
route-state + dry-run trio (`show_route_state`, `clear_route_state`,
`dry_run_route`). Same bearer-token auth throughout. Multi-host
fan-out (Valkey pub/sub) lands in a follow-up. The user-facing
skill (slice 12) ships at `skill/wiremirage/` (with a debug
sub-skill at `skill/wiremirage-debug/`) — `SKILL.md` + 3
ready-to-run scripts teaching the CLI workflow. Slice 14 added
admin user CRUD to the CLI (`wm users
list/show/me/create/update/delete`) and `wm completion <shell>` for
bash/zsh/fish/powershell. Slice 15 added route update: `PATCH
/__api/routes/{group}/{n}` plus the matching `wm routes update` CLI
subcommand and `update_route` MCP tool. Mutable fields are
`methods`, `path`, and the artifact triple (`source`/`compiled_wasm`
+ `language` + `bindings_version`); path or method changes
re-validate pattern conflicts (excluding self) and swap the
by-method-path index, and any wasm swap evicts the RouteTable's
component cache for that route. MCP stays wasm-only on the
artifact, matching `create_route`. Slice 16 added per-route state +
dry-run: `GET/DELETE /__api/routes/{group}/{n}/state` for listing
and clearing the route's private kv, plus `POST
.../{n}/dry-run` for running the handler against a synthetic
request. Dry-run snapshots `kv:` and `gkv:` to `dryrun:{run_id}:` so
state writes are isolated and discarded on completion; the journal
is untouched. The CLI wraps both as `wm routes state` (list /
`--clear`) and `wm routes test`. MCP exposes `show_route_state`,
`clear_route_state`, and `dry_run_route` (all owner-or-admin).
Slice 17 added activity tracking — `hits_total` + `last_hit_at`
on every route record, `last_activity_at` on every group record.
Bumped by the dispatch path on every matched request (two `HSET`s
+ one `HINCRBY` per match; best-effort like the journal write).
The fields surface in REST / wm-core / MCP responses; sort-by-
activity on list endpoints is the next slice (REST list-surface).
Slice 18 added the REST list-surface: shared filter/sort/pagination
across `GET /__api/routes`, `/__api/groups`, `/__api/journal/{group}`,
and `/__api/unmatched`. Routes/groups use offset pagination
(`?offset=&limit=`, response `{ ..., total, next_offset }`) with
sort columns `created_at` / `last_hit_at` / `hits_total` for routes
and `created_at` / `name` / `last_activity_at` for groups.
Journal/unmatched keep cursor pagination and gain `method`,
`path_pattern` (a `*`-glob), `status`, `since` / `until`, plus
`route` on the journal endpoint. The shared parsing lives in
`crates/wm-host/src/api_filters.rs`; the shared matcher is
`JournalFilter` (extended with `since` / `until`, path_pattern now
glob-matched, used by SSE tail + journal list + unmatched list).
Validation failures surface `code: validation_failed` with
`diagnostics: ["parameter=<name>"]`; a non-admin passing `owner_id`
returns 403. wm-core gains `ListRoutesParams` / `ListGroupsParams`
/ `ListJournalParams` / `ListUnmatchedParams` plus
`Client::list_*_with(params)` methods (no-arg variants kept as
forwarders). Slice 19 wraps slice 18 in the CLI and MCP: `wm
groups list`, `wm routes list`, `wm journal list` each gain the
matching flags (`--method`, `--path-pattern`, `--since`, `--sort
last_hit_at`, `--limit`, `--offset`, etc.) and human output prints
a `(showing K of N; --offset M for the next page)` footer. A new
`wm unmatched list` (admin-only) covers the host-wide unmatched
view, with `wm unmatched show <n>` for individual records. MCP
tools `list_groups`, `list_routes`, `list_recent_unmatched` gain
the same arg fields; non-admin still pinned to self. The route /
group sort comparators are promoted to `pub(crate)` in `api.rs`
so both surfaces share them. Slice 20 added local auth + browser
sessions per ADR-0018: `WM_LOCAL_AUTH=alice:hunter2:admin,bob:pw`
declares users (argon2id-hashed at startup, never persisted),
`POST /__auth/login/password` mints a `wm_session` cookie signed
by `SESSION_SECRET` (HMAC-SHA256, ≥32 bytes), and the auth
extractor accepts the cookie as a fallback to bearer tokens.
Sessions live at `session:{token}` in Valkey with 24h sliding TTL;
logout deletes the record and clears the cookie. Per-IP login
throttle (5 fails / 60s → 60s lockout) lives in-process. ADR-0018
is the scope statement — testing + trusted-network deployments
only, not for public exposure. Slice 21 kicked off the web UI:
templates via `minijinja` (compile-time-embedded via
`include_str!`), a CSS stylesheet implementing the design tokens
from `web-ui-design.md` (light + dark mode), a base layout shell
with primary nav, a login-page rewrite from inline HTML to the
template, a real home page (`/__ui/`) showing the user's groups,
and stub pages for every remaining `/__ui/*` route so navigation
works end-to-end. Auth-redirect middleware on `/__ui/*` sends
unauthenticated browsers to `/__auth/login?next=...`; the password
form already honoured `next`. `just run-web` boots the host with
sensible dev creds (`admin/devpassword`) — visit
`http://localhost:8080/__ui/` to dogfood. The 13 remaining
screens are the rest of the UI track (slices 22–26); OAuth lands
in slice 27. Slice 22 added the Groups + Routes list pages
(`/__ui/groups`, `/__ui/routes`) on top of the slice-18
filter/sort/paginate surface. The REST handlers `list_routes` and
`list_groups` were refactored into thin wrappers over new
`pub(crate)` helpers `list_routes_core` / `list_groups_core` so
the UI handlers share the exact filter and ownership-scoping
path. UI affordances on top of the API: a `owner_scope=mine|
everyone` toggle (admin-only, defaulting to "everyone") in place
of raw `owner_id`, sort-toggle column headers that flip asc/desc
on the active column, 25-per-page pagination with prev/next
links, and a 400 placeholder page on a bad filter parameter.
Templates `groups_list.html` and `routes_list.html` extend the
slice-21 layout shell; CSS adds `.filter-form`, `.filter-field`,
`.btn--ghost`, `.btn--disabled`, `.pagination`. Owner column
resolves user ULIDs to usernames via a single batched lookup per
page. Slice 23 added the Group + Route detail pages
(`/__ui/groups/{group}`, `/__ui/routes/{group}/{n}`) — both are
read-only reflections of the underlying records, with breadcrumb
nav back to the list pages, an owner-or-admin authorization gate
(403 for non-owners, 404 for unknown), and a "Manage from CLI"
panel listing the equivalent `wm` commands until the CSRF-enabled
authed-action slice lands. Route detail surfaces a short tail of
recent journal entries (≤10) filtered to this route. Slice 23
also implemented the bare-`/` redirect from route-model.md: an
unmatched `GET /` bounces to `/__ui/` (with a valid `wm_session`
cookie) or `/__auth/login` (without), wired into `dispatch_inner`
so a user-registered `GET /` route still shadows it. The redirect
does NOT write to the unmatched journal — a human pointing a
browser at the host isn't a "missing mock" signal. Slice 24
added the journal screens. `/__ui/journal/live` pre-fetches
~25 most-recent entries server-side (when scoped to a group) and
opens an `EventSource` against `GET /__api/journal/tail` — the
slice-11 SSE endpoint — to prepend new rows as `handled` events
arrive. Plain JS, no HTMX yet (single stream + append is not
worth pulling in the runtime). Host-wide pre-fetch (admin
without `?group=`) fans out across every group, unions their
20 most-recent entries, sorts desc by `created_at`, and
returns the top 50 so the page is populated on revisit, not
just on new traffic. Group-scoped pre-fetch reads from the
group's journal directly. Pre-fetch window is generous (200
raw entries) so narrow filters still tend to have content
after a reload. Group detail page (slice 23) now carries the
same live pane scoped to that group via `?group=` — same
EventSource pattern, ~10 most-recent entries pre-rendered.
Slice 25 added the self-service tokens page (`/__ui/me/tokens`,
list / create / revoke own tokens, plaintext shown exactly once
on create) and the CSRF middleware that protects every authed UI
form. CSRF uses double-submit cookies: middleware on `/__ui/*`
and `/__auth/*` mints a `wm_csrf` cookie on safe methods (stored
HttpOnly, SameSite=Strict, 24 h) and validates `_csrf` form
field against the cookie on POST/PUT/PATCH/DELETE. A
`tokio::task_local!` carries the current token through the
request scope; the `ui::render` helper merges `csrf_token` into
every template context via `minijinja::context!`'s spread
syntax, so handlers don't have to plumb it through. The login,
logout, and tokens forms all embed `<input type="hidden"
name="_csrf" value="{{ csrf_token }}">`. Filter + group dropdown carry
through to the SSE URL via `build_sse_url`. Authorization
mirrors the SSE endpoint: with `?group=` the caller must be
admin or own a route in that group; without it, admin-only
(non-admin renders a group-picker with no SSE connection).
`/__ui/journal/{group}/{n}` renders the full journal record —
request envelope, response envelope, handler logs, timing,
trace ID — with the same owner-or-admin gate. Binary bodies
render as `(binary, N bytes)`; text bodies render verbatim with
a truncated-warning if the journal had to trim them. minijinja
gains the `json` feature for the `tojson` filter used to embed
the SSE URL in the inline script safely. Slice 26 wired action buttons onto the detail pages now that
CSRF is online: `POST /__ui/groups/{group}/refresh|edit|delete`
for the group lifecycle (refresh TTL, edit TTL + sliding flag,
cascade-delete) and `POST /__ui/routes/{group}/{n}/delete` for
routes. All owner-or-admin-gated; the registry's
`refresh_group`, `patch_group`, `cascade_delete_group`, and
`delete_route` are called in-process. Templates' "Manage from
CLI" panels became real `.action-row` button blocks with a
`confirm()` prompt on destructive actions and an `<details>`
edit-TTL disclosure. New CSS: `.btn--danger`, `.action-row`,
`.edit-disclosure`, `.filter-checkbox`. Route source editing,
the "+ Add route" button, and the dry-run modal are still
deferred. Slice 27 added the state inspection pages:
`/__ui/routes/{group}/{n}/state` lists a route's private
`kv:` namespace (key, kind, value or size) and `/__ui/groups/
{group}/state` lists the group's shared `gkv:` namespace; both
POST to the same URL to clear (the group page wipes both `kv:`
and `gkv:` for the group, matching `cascade_delete_group`'s
state-side semantics). New registry helper `list_group_state`
mirrors `list_route_state` but reads from `storage.group_bucket`.
Owner-or-admin-gated; non-owner → 403, unknown → 404. UI
shows kind-aware previews: bytes render as UTF-8 text when
clean (with byte-size annotation), otherwise as `binary, N
bytes`; lists/sets/hashes show their length. Templates
`route_state.html` and `group_state.html` plus an "Inspect
state" `.btn--ghost` on the matching detail page's
`.action-row`. Tier-2 coverage: `tests/ui_state_pages.rs`
exercises empty state, list-after-dispatch (driving the
counter_handler fixture), clear-state redirect + wipe, 403
non-owner, 404 unknown, plus the group-clear-also-wipes-route-
state semantics. Slice 28 promoted the `/__ui/unmatched` stub
into the admin-only unmatched view: a list page with
method + path-pattern filters and cursor pagination over
`?before=`, plus `/__ui/unmatched/{number}` for the request
envelope (headers + body). Reuses
`JournalFilter::matches_unmatched` for filtering so the UI
agrees with `/__api/unmatched` semantics. Per-row links go to
`/__ui/unmatched/{n}` for the detail and to
`/__ui/routes/new?method=…&path=…` for create-from-request
(target still stubbed). Tier-2: `tests/ui_unmatched_pages.rs`
covers empty / lists / method-filter / path-glob-filter / bad
method 400 / pagination cursor / detail body / detail 404 /
non-admin 403 on both pages. Slice 29 added the
`/__ui/routes/new` route-creation form. GET renders the form
(method/path/group/language/source) and honours
`?method=&path=&group=` prefill, so the unmatched-page deep
link from slice 28 now lands somewhere real. POST shares the
create pipeline with `POST /__api/routes` via a new
`api::create_route_core` helper extracted from the REST
handler — same validation, same compile-failure surface, same
component-validation step. The UI form is source-only
(TypeScript / JavaScript); pre-compiled wasm uploads stay on
the REST surface where a bytes body makes sense. On success
the user is 303'd to `/__ui/routes/{group}/{number}`; on
failure the form re-renders with `error.title` / `message` /
`diagnostics` from the `ApiError` and returns 400 with the
submitted values preserved. CSRF on the POST. Tier-2:
`tests/ui_route_new.rs` covers GET defaults / GET prefill /
POST happy path / reserved-path rejection / bad-source
compile_failed / missing-CSRF 403. Slice 30 cleaned up two pieces of wireframe
drift the dogfood pass surfaced: group-detail had a separate
"Manage" card at the bottom of the page (slice-26 layout)
instead of the wireframe's inline header for Refresh/Edit TTL
+ footer for Full journal/Group state/Delete; and
`/__ui/routes/new` was using `.filter-form`'s horizontal flex
row instead of the wireframe's 2-column label/input grid +
dedicated "Handler source" section. Both pages were
restructured to match the wireframe (modulo the slice-24 call
to keep the Live activity pane as a full-width card below
routes rather than a right-column aside — the wireframe was
updated to reflect that). Discoverability for the route-new
form: "+ Add route" button on group detail's Routes section
(pre-filling `?group={name}`), "+ New route" button next to
the Routes list H1, and the empty-state copy on group detail
links to the form directly. New CSS: `.form-grid`,
`.source-editor`, `.page-footer`, `.page-header__row`. Slice
31 was an audit-driven cleanup that bundled five small
fixes across the UI surface to match the wireframes: route-
detail layout sync (metadata in header, footer row with
Route state · Run dry-run · Delete route, retiring the
slice-26 "Manage" card); tokens page polish (TTL preset
dropdown — Never/30d/90d/1y/Custom — plus sortable column
headers); token rename end-to-end (new
`Auth::rename_token`, REST `PATCH /__api/tokens/{name}`, UI
form per row with a `prompt()` for the new name); journal-
entry layout sync (breadcrumb walks Groups → group → route
→ #N, Status/Duration/Trace move into the header `<dl>`,
dropped reserved headers collapse into a `<details>`
inside Response, handler errors promote to a `.card--error`
callout above Request, Summary card retired); and a small
leftovers commit that switched the routes-list Group
filter from a text input to a `<select>` of the caller's
groups and added a `← Back to {group/route}` link in the
state pages' footer to soften the destructive Clear
button. Slice 32 added the dry-run UI page at
`/__ui/routes/{group}/{n}/dry-run` — a real full page (not
a JS modal) with a form for method/path/headers/query/body
that calls `dry_run::dry_run` directly and re-renders with
a Response card showing status pill, duration, snapshot
key count, headers, body, handler logs, and any handler
error. Owner-or-admin gated; CSRF on the POST. The route
detail page's footer "Run dry-run" link is now real (not
the slice-30 "CLI only" placeholder). Eight tier-2 tests
including verification that dry-run touches neither the
route's real kv nor the journal. Slice 33 added dry-run
seed state across all surfaces: `DryRunRequest` gains
`kv_overrides` + `gkv_overrides` maps that the snapshot
machinery applies *after* the real-state deep-copy and
*before* the handler runs, letting agents test
state-dependent branches (`if counter > 3`) without
driving real traffic first. REST takes Vec<u8> values
(array-of-ints JSON, matches `body` field); MCP
`dry_run_route` takes `kv_overrides_b64` /
`gkv_overrides_b64` as base64-encoded string values
(matches the `body_b64` convention); CLI `wm routes test`
gains `--kv KEY=VALUE` / `--gkv KEY=VALUE` repeatable
flags (UTF-8 bytes); the UI dry-run page adds two
textareas under a "Seed state" card with `key=value` per
line. Real state is never touched — overrides land in the
disposable `dryrun:{run_id}:` namespace. Bytes-only:
list/set/hash seeding is deferred (the workaround is to
seed via real traffic before dry-run). Slice 34 added
Pause/Resume to `/__ui/journal/live` — pure client-side
JS that buffers incoming SSE events (capped at 500) while
paused and flushes them oldest-first on resume so the
table order matches the un-paused stream. The status
indicator reports `paused · N buffered` while paused so
operators can see traffic without losing it. Slice 35
populated `UnmatchedRecord.near_misses` (which had always
been `vec![]`): the dispatcher's unmatched-write path now
calls `RouteTable::compute_near_misses(method, path)` —
the same probe slice 13's `find_route` runs — and stores
the slim `UnmatchedNearMiss { route, route_path,
route_methods, reason }` records on the journal entry.
Reason carries either `MethodMismatch { expected_methods,
got }` or `PrefixMatch { segment_index, expected, got }`.
The UI's unmatched list page now shows a "Did you mean
…?" hint per row (or "No close neighbours." when empty),
and the detail page lists every near-miss with an
explanation; REST `/__api/unmatched/{n}` and MCP both
serialise the same shape. Slice 36 added source storage on
the registry: the `Route` record gains
`source: Option<String>` alongside `compiled_wasm`,
populated for source-language uploads and `None` for
pre-compiled wasm. New endpoint
`GET /__api/routes/{group}/{n}/source` (owner-or-admin)
returns `{ slug, language, source }`; MCP exposes the
same as `show_route_source`; the CLI as
`wm routes source <slug>`. Like `compiled_wasm`, the
source is never inlined on list/get responses — only the
dedicated endpoint returns it. Wasm swaps via PATCH clear
any stored source; source-language swaps overwrite it.
Sets up the route-source viewer slice on the UI. Slice 37
landed that viewer: `/__ui/routes/{group}/{n}` now renders a
"Handler source" card just above the footer. Source-language
routes show the stored source in a read-only
`<pre class="source-block">` block; wasm-uploaded routes show
"No source stored — route was uploaded as pre-compiled
`{language}` ({size} component)." Replaces the slice-23
placeholder paragraph. No new endpoint — the source already
travels on the Route record after slice 36, and the detail
page is already owner-or-admin-gated. Slice 38 closed the
slice-35 MCP deferral: `list_recent_unmatched` now ships
the slim `near_misses` list on every `UnmatchedSummary`
entry, so agents see the "Did you mean…?" candidates
without a second REST hop. Empty when no neighbour matched
(present as `[]`, not omitted, so callers can rely on the
field shape). Slice 40 added source editing on the
route-detail UI: a new `/__ui/routes/{group}/{n}/source/edit`
page renders a textarea pre-populated with the stored
source; POST forwards to `api::patch_route_core`
(extracted from the REST handler), which runs the in-host
TypeScript transpile (slice 58) and swaps the artifact in
place. Compile
errors re-render the form with diagnostics and the
user's edits preserved; success redirects back to the
detail page. wasm-uploaded routes (`source: None`) 404
on this page rather than offer a misleading affordance.
By design (per `mcp-surface.md`) user management is **not**
in MCP — admins handle it via CLI/UI. Slice 41 added Ace
Editor to the source viewer + editor: `route_detail.html`
renders read-only, `route_new.html` and
`route_source_edit.html` give a real editor with line
numbers, indentation, and JS/TS syntax highlighting. Ace
is vendored under `src/ui/static/ace/` (core + JS + TS
modes + light/dark themes) and served through the
existing `/__ui/static/*` enum-match handler. A small
`wm-ace.js` bootstrap finds `data-wm-ace` divs, syncs
into a hidden `<textarea>` for form submit, and flips
theme on `prefers-color-scheme` changes. No JS bundler;
script-tag distribution only. Slice 42 unblocked the
agent-driven deployment shape: MCP `create_route` and
`update_route` now accept `source` + `language`
(`typescript` / `javascript`) alongside the existing
`compiled_wasm_b64` path. Both handlers delegate to
`api::create_route_core` / `patch_route_core`, so the
in-host transpile, slug-conflict precheck, and source-storage
behavior are identical to what REST does. Compile failures
surface back to MCP as `compile_failed` with diagnostics in
the `data` payload. The slice-10 wasm-only carve-out is
retired — agents no longer need a wasm toolchain to register
TS/JS handlers. Slice 43 closed the MCP/CLI/UI parity gap on
group editing with a new `update_group` MCP tool — agents can
now flip `ttl_seconds` and `sliding_ttl` on a group they
own without dropping to the CLI. Owner-or-admin only, same as
the REST PATCH; rename and owner-transfer remain out of scope.
Slice 44 wired two pre-deploy hardening flags. `WM_SECURE_COOKIES=1`
appends `Secure` to the `wm_session` + `wm_csrf` cookies for
deployments behind a TLS edge; default off keeps plain-HTTP dev
workflows working. `WM_TRUST_FORWARDED_HEADERS=1` honors
`X-Forwarded-For` for the per-IP login throttle; default off
(loopback placeholder) so a directly-reachable host can't be
IP-spoofed. The CSRF middleware now takes `AppState` via
`from_fn_with_state`. README gets a "Production hardening" section
covering the two flags plus bootstrap-token rotation, strong
`SESSION_SECRET`, edge HSTS/CSP, and binding the host to localhost.
Slice 45 capped the mock-dispatch request body at the design value
of 10 MiB (`storage-model.md::limits.request_body_size`). Above the
cap returns 413 *before* the handler runs and *without* writing to
the journal — junk floods don't pollute logs. The `/__api/*` JSON
path got its axum default lifted from 2 MiB to 16 MiB so wasm
uploads on `POST /__api/routes` + `PATCH /__api/routes/{g}/{n}`
fit comfortably. The auth-gated surface keeps the dispatch cap
the only public-facing limit; the larger API limit only applies
to authed callers. Slice 46 wired ADR-0002's wasm sandbox
limits: `Engine` config gets `consume_fuel(true)` +
`epoch_interruption(true)`, every `Store` is set up with a
10 B fuel budget, a 100-tick epoch deadline (≈1 s wall via
the 10 ms epoch ticker spawned at host startup), and a
`HandlerLimits` resource limiter that caps linear memory at
64 MiB and tracks the peak for the journal. Whichever limit
fires first traps the call; the existing handler-error path
journals it. `ResourceUsage::fuel_consumed` and
`memory_peak_bytes` go from 0-placeholders to real numbers
captured from the store before it drops. Slice 47 added
ADR-0024 OTLP metrics over the existing tracing pipeline:
`OTEL_EXPORTER_OTLP_ENDPOINT` now toggles BOTH traces and
metrics through one endpoint; a new `metrics` module
defines a fixed catalog of `wm.dispatch.*` (duration,
active_requests, request_body_bytes), `wm.handler.*` (fuel,
memory, wall, traps_total{reason}), and `wm.streaming.*`
(head_latency, duration, chunks/bytes, terminations
{disposition}). Mock traffic only; mock-metric
cardinality stays bounded by small enums × HTTP method ×
status — no route / group / user labels by design, with the
allowlist enforced in the smoke test
(`metrics_smoke.rs`). Per-route mock detail is the product
surface (slice 17 onward) for at-a-glance counts, and traces
for distributional slicing — not mock metrics. Slice 2 added
the control-plane HTTP metrics: a `route_layer` middleware on
the internal sub-routers (api / auth / ui) records OTel
HTTP-semconv `http.server.{request.duration,active_requests,
request.body.size}` keyed by `{method, status, http.route,
wm.surface}` — `http.route` is the matched *template* (via
`MatchedPath`, so path params never explode cardinality), and
the internal route set is bounded by code so the route label
is operator-safe. The MCP streamable endpoint + the
`/__health` / `/__ready` probes are intentionally not
recorded. Slice 2 also enriched the dispatch span with
`handler.fuel_consumed` / `memory_peak_bytes` / `wall_ms`
(buffered) and `streaming.head_latency_ms` (streaming) so
per-route resource questions are trace queries rather than
high-cardinality metric labels. README "Observing the host"
section captures the operator playbook. A follow-up added two
more obs pieces (ADR-0024 amendment): MCP per-tool
instrumentation — a hand-written `call_tool` on the
`ServerHandler` impl (the `#[tool_handler]` macro only generates
one if absent) wraps the same `tool_router.call` dispatch with an
`mcp.tool` span + `wm.mcp.tool.{calls_total,duration_ms}` metrics,
tool label bounded by `tool_router.has_route` (unknown →
`"unknown"`); and control-plane request spans — the
`internal_http_metrics` middleware now also opens an
`http.server.request` span (method / route-template / surface /
status) so API/UI/auth traffic appears in traces with per-route
latency (the arkiv-parity piece that lets a trace backend answer
"p95 for `/__api/groups/{group}`"), static assets excluded from
spans for volume. The histogram aggregation is explicit-bucket,
not exponential (ADR-0024 amended): consumed for sum/count
(rate/mean) only — Logfire stores no sum/count for exponential
histograms, and metric-side percentiles aren't relied upon.
Slices 56–58 implemented
ADR-0020: a shared `js-engine.wasm` (componentize-js bundle of
StarlingMonkey + a small dispatch shim) lives under
`compiler/js-engine/`. Dispatch on `language: "javascript" |
"typescript"` instantiates a fresh component per request and reads
the matched route's source through a `get-source` host import.
TypeScript transpiles to JS in-host via pure-Rust swc
(`crate::ts_transpile`) before storage — TS and JS now share a single
dispatch path with no Node sidecar. `WM_COMPILER_URL` is gone;
`docker-compose.yml` no longer ships a `compiler-typescript`
service; `compiler/typescript/` is deleted. Slice C of ADR-0020
made the engine a build-time artifact rather than a vendored blob:
`crates/wm-host/build.rs` runs `compiler/js-engine/Dockerfile`
(pinned `node:22-bookworm-slim`) to produce `js-engine.wasm`
inside cargo's `OUT_DIR`, stamps the path into
`WM_JS_ENGINE_WASM`, and `src/main.rs`
`include_bytes!`'s it. Docker is now a build dependency;
`WM_JS_ENGINE_WASM_OVERRIDE=/abs/path` skips docker for
release-image builds or no-Docker contributors. Nothing is checked
in under `crates/wm-host/vendored/` (the directory is gitignored).

## Where the design lives

The design is captured as docs and ADRs in a private Arkiv workspace named
`wiremirage` (search via `mcp__claude_ai_Arkiv__search_workspaces` to find
the ID). Read with the `mcp__claude_ai_Arkiv__*` tools.

**Treat Arkiv as read-only by default.** Don't write/edit/delete files in
the workspace unless the user has explicitly asked you to.

Key documents to load early when working on a task:

- `index.md` — entry point and document map
- `architecture-overview.md` — components, request flows, deployment
- `adrs/index.md` — list of decision records
- The specific design doc the task touches (e.g., `route-model.md`,
  `storage-model.md`, `script-api-wit.md`, `cli-design.md`)

## Repo layout

Cargo workspace with three crates under `crates/`:

- `wm-core` — shared types, REST client, auth
- `wm-host` — long-running Rust server (axum + wasmtime + Valkey).
  MCP service is a `mcp/` module here, mounted at `/__api/mcp`.
- `wm-cli` — `wm` CLI binary

Plus a non-Rust subdirectory used at build time only:

- `compiler/js-engine/` — TypeScript shim + Dockerfile that produces
  the shared `js-engine.wasm` component (ADR-0020). Built at cargo build
  time by `crates/wm-host/build.rs` via the pinned
  `node:22-bookworm-slim` image; the resulting wasm lands in cargo's
  `OUT_DIR` and is embedded into the host binary via `include_bytes!`.
  **Not** in the cargo workspace. Source-language handler dispatch goes
  through this shared engine. TypeScript → JS happens in-process in the
  Rust host via swc, not here.

The WIT contract that handlers program against lives at `wit/wiremirage.wit`.
It is the verbatim mirror of `script-api-wit.md` in the Arkiv workspace; if
you need to change it, update the design doc first.

Wasm guest fixtures used by the host's tier-2 integration tests live at
`crates/wm-host/tests/fixtures/<name>/` as **standalone crates** (their own
`Cargo.toml` + `Cargo.lock`, excluded from the parent workspace). The host's
`build.rs` compiles them to `wasm32-unknown-unknown` and runs `wasm-tools
component new` on the result; the resulting paths are stamped into env vars
of the form `WM_FIXTURE_<name>_COMPONENT` for tests to read via `env!()`.

Conformance tests live at `conformance/<name>/` — opt-in, manual lanes that
run a real third-party client library against a WireMirage mock to smoke out
fidelity gaps (SSE framing, content-type, error-body shape) the unit suite
can't see. Lanes so far: `conformance/openai-streaming/` (the real `openai`
Python client vs a mocked `POST /v1/chat/completions`, streaming + buffered) and
`conformance/s3-slowdown/` (the real AWS Go SDK vs a **reusable, config-driven
latency/throttle-injection** mock; proves the SDK auto-retries/recovers from
injected `503 SlowDown`). Each lane is a dir with a `Dockerfile` (its
language/SDK toolchain, pinned), a `routes.json` (sources → paths, optional
`group`), the mock handler(s), an optional `setup.sh` (post-registration
seeding), and the client test. The shared `conformance/run.sh` boots the host
in-memory (native, cargo), registers a lane's routes via jq/curl, runs any
`setup.sh`, and runs the lane's client **in Docker** (`--network host`) — so the
host machine needs only Docker + jq + a buildable host, no per-language
toolchain. Run with `just conformance [lane]` or `conformance/run.sh [lane]`
(no arg = all lanes). Note: a reusable runtime-configurable mock seeds its
config *through a mock route* (the s3 lane's `config.ts`) because there's no
public kv/gkv-write API — only GET/DELETE. CI lane is
`.github/workflows/conformance.yml` (`workflow_dispatch` only — not gating,
since it builds + boots the host and builds SDK images). Not in `just check`.

The product skill (shipped to *users* of WireMirage) lives at
`skill/wiremirage/` per ADR-0015 (with a debug sub-skill at
`skill/wiremirage-debug/`). The dev skill at `.claude/skills/wm-dev/`
is for *developing this repo* — not the same thing.

**The product skill is tightly coupled to the current CLI surface.**
Any time you add, rename, or remove a `wm` subcommand or flag — or
change the handler API, route shape, etc. — check `skill/wiremirage/`
(SKILL.md + scripts/*.sh) and update what's affected in the same
change. Same for `skill/wiremirage-debug/SKILL.md`. The skill goes
stale fast and "describe the current surface" is the only commitment
worth making.

## Common commands

Use `just` (see `justfile`):

- `just check` — fmt check + clippy `-D warnings` + tests (skips Docker tests)
- `just check-all` — like `check` plus tier-3 Valkey tests
- `just fmt` — format
- `just test` — workspace tests only (no Docker)
- `just test-valkey` — tier-3 Valkey-backed tests, requires Docker
- `just build` — `cargo build --workspace`
- `just run-host` / `just run-cli <args>`

To run the host with Valkey:

```sh
docker compose up -d   # starts valkey
WM_BOOTSTRAP_TOKEN=wmt_dev_local \
  WM_STORAGE=redis://localhost:6379 \
  cargo run -p wm-host
```

Or in-memory:

```sh
WM_BOOTSTRAP_TOKEN=wmt_dev_local WM_STORAGE=memory cargo run -p wm-host
```

Register a TypeScript route and call it:

```sh
curl -X POST localhost:8080/__api/routes \
  -H 'authorization: Bearer wmt_dev_local' \
  -H content-type:application/json \
  -d '{
    "methods": ["POST"],
    "path": "/v1/charges",
    "language": "typescript",
    "source": "function handle(req,_r,_g){return {status:200,headers:[],body:new TextEncoder().encode(\"hi from \"+req.method)};}"
  }'
# Mock traffic does not need an Authorization header.
curl -X POST localhost:8080/v1/charges -d '{}'
```

Env vars (no silent fallbacks; missing required → fail-fast):

- `WM_STORAGE` (required) — `memory`, `redis://...`, or `rediss://...`
- `WM_BOOTSTRAP_TOKEN` (required on first startup, optional on
  restarts once at least one user exists) — plaintext for the admin
  user named `bootstrap`. Treat like a credential.
- `WM_SECURE_COOKIES` (optional, slice 44) — `1`/`true`/`yes`/`on`
  to append `Secure` to the `wm_session` + `wm_csrf` cookies. Off by
  default so dev workflows over plain HTTP keep working; required
  for deployments behind a TLS edge.
- `WM_TRUST_FORWARDED_HEADERS` (optional, slice 44) — `1` to honor
  `X-Forwarded-For` for the per-IP login throttle. Off by default
  so a directly-reachable host can't be spoofed; required for
  deployments behind a reverse proxy that populates XFF.
- `OTEL_EXPORTER_OTLP_ENDPOINT` (optional) — URL of an OTLP/gRPC
  collector (e.g. `http://localhost:4317`). When unset, the host logs
  to stderr only; when set, spans are exported in addition.
  `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` are honored too
  (standard OTel SDK behavior).

## Required tooling

In addition to a stable Rust toolchain:

- `wasm32-unknown-unknown` target — `rustup target add wasm32-unknown-unknown`
- `wasm-tools` CLI — `cargo install wasm-tools` (used by `wm-host/build.rs`
  to componentize fixture guests; also handy for `wasm-tools component wit
  <component.wasm>` when investigating component-shape issues)
- `just` — `cargo install just`
- **Docker** — required for `cargo build` (the host's `build.rs`
  invokes `compiler/js-engine/Dockerfile` to produce the shared
  `js-engine.wasm` component; pinned `node:22-bookworm-slim`, image
  layer-cached). Also required for the tier-3 testcontainers suite
  (`just test-valkey`). Set `WM_JS_ENGINE_WASM_OVERRIDE=/abs/path` to
  skip the docker invocation and use a pre-built artifact, e.g. for
  release-image builds or restricted CI lanes.

## Conventions

- Latest stable Rust, edition 2024. No MSRV pin.
- Clippy is `-D warnings` in CI; fix lints rather than allowing them.
- Significant design decisions go in an ADR before implementation; ADRs
  live in Arkiv at `adrs/NNNN-slug.md` and follow the structure documented
  in `adrs/index.md`.
- License is Apache-2.0. New source files don't need a header (the LICENSE
  file at the repo root covers them).
- Don't add `_unused` renaming, "kept for backwards compat" shims, or other
  decorative scaffolding when refactoring — delete the dead code.

## Slash commands

- `/check` — runs the full check suite and reports
- `/new-adr` — scaffolds a new ADR in the Arkiv workspace following the
  established conventions

## Subagents

Prefer `Explore` for codebase searches, `Plan` for non-trivial design
work. The `claude-code-guide` agent handles questions about Claude Code
itself. There are no repo-tuned subagents yet.
