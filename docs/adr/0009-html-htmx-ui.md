# ADR-0009: Server-rendered web UI

**Status:** Accepted

**Context:** WireMirage is agent-driven by design — the MCP server and REST API cover the create/inspect/modify/delete loop without a UI. But humans still need to see what their agents have created, debug "why isn't this working" cases, and inspect the journal. A UI is genuinely useful even in an agent-first tool; without one, evaluation, demos, and ad-hoc debugging all suffer.

The UI's scope is deliberately narrow:

- See lists of groups and routes (mine, all-readable)
- View a route's source, config, and recent journal entries
- Edit a route (with the same compile-and-validate pipeline as the API)
- Delete a route, change a group's TTL
- See unmatched requests, with a "create route from this" shortcut
- A minimal route-creation form for ad-hoc fixes
- Manage the user's own API tokens
- Login flow

It explicitly does **not** include:

- Sophisticated authoring (no LSP-grade IDE, no in-browser type checking, no autocomplete)
- Templating helpers, response builders, or other handler-authoring DSLs
- Team management beyond the basics
- Analytics, dashboards, or charts

Three implementation approaches were considered:

1. **Server-rendered HTML** with a small amount of client-side JS for the bits that genuinely need it (live journal stream, code-editor wiring). Templates rendered in the host process.
2. **Single-page application** (React, Vue, or Svelte) bundled and served as static assets by the host.
3. **Desktop or hybrid app** (Tauri, etc.). Wrong shape for a service that's designed to run on the network.

**Decision:** Server-rendered HTML with **minijinja** for templates (Jinja-style runtime templates embedded into the binary via `include_str!`). The UI is mostly full-page navigation; the small handful of places that need client-side behaviour use vanilla JS without an extra framework. Streaming surfaces (the live journal tail) use the browser's built-in `EventSource` against the server's SSE endpoint.

**Consequences:**

- **No frontend build pipeline.** The Rust binary embeds the templates and static assets at compile time and serves them. No npm, no Vite, no separate frontend repo.
- **Templates live alongside Rust code.** `crates/wm-host/src/ui/templates/*.html` are loaded via `include_str!`, parsed once at startup, and rendered on every request. minijinja's syntax is the Jinja-flavoured `{% extends %} / {% if %} / {{ var }}` that contributors coming from Python or JS ecosystems already know.
- **Server is the source of truth.** Every state-changing operation goes through the same REST API the agent uses; the UI is just another client. State doesn't drift between "what the UI thinks" and "what the server thinks."
- **No HTMX.** The live journal feed uses plain `EventSource` against the SSE endpoint and appends rows in vanilla JS; the dry-run page is a full POST/render cycle; form submissions are full-page navigation. The places where HTMX would add value (in-place partial updates, fragment swaps) we don't have. Pulling HTMX in for two SSE listeners would be net overhead.
- **Cost: no compile-time template checking.** A typo in `{{ data.namee }}` is a runtime miss, not a compile error. Mitigated by tier-2 UI tests that render every page server-side and assert key fragments — broken bindings fail in CI rather than in a user's browser. The original draft of this ADR proposed maud for its compile-time checks; the team chose minijinja in slice 21 for the lower learning curve and easier template authoring, accepting the runtime-error trade-off.
- **Cost: limited interactivity.** Anything that requires client-side state (multi-step wizards, drag-and-drop, optimistic UI) is harder than in an SPA. We accept this — our UI is mostly forms, lists, and detail views, which fit a server-rendered model well.
- **Source viewing and editing uses Ace Editor.** Bundled into the host binary as static assets under `src/ui/static/ace/` (core `ace.js`, the JavaScript + TypeScript syntax modes, and light/dark themes), served through the existing `/__ui/static/*` handler. Read-only on the route detail page, editable on the create + source-edit pages. This is a v1 feature, not deferred — syntax highlighting on handler source meaningfully improves the read-and-verify loop for both humans and (when an agent inspects the rendered UI) AI clients. LSP-grade editing (in-browser type checking, autocomplete, jump-to-definition) is explicitly out of scope. Ace is BSD-licensed, ships as plain JS files (no npm tooling), and the bundle is small enough that we send it on every page — no `textarea` fallback. The original draft of this ADR named CodeMirror 6 here; slice 41 picked Ace for the simpler vendoring story (script tags, no bundler).

**Alternatives considered:**

- **maud (compile-time Rust templates).** Same general idea as minijinja but with Jinja-shaped templates baked into the Rust source via macro. Catches template variable typos at `cargo build` time. Rejected in favour of minijinja for ergonomics — the templates are small, the runtime-error risk is mitigated by tests, and the contributor-onboarding cost of "learn maud's Rust-shaped syntax" outweighed the typo-catching benefit.
- **askama.** Another compile-time Rust template engine, Jinja-syntax in separate files. Same trade-off as maud. Rejected for the same reasons.
- **Tera (runtime templates).** Jinja-style, evaluated at runtime. Functionally similar to minijinja; minijinja is smaller and has slightly tighter Rust ergonomics. Either would have worked; minijinja was the pick.
- **HTMX as the interactivity layer.** Considered initially (the original draft of this ADR named HTMX explicitly). Rejected during implementation: we don't have a need for in-place partial-fragment swaps — the live-journal stream is the only piece that warrants any JS, and `EventSource` covers it directly. HTMX is a reasonable fit if the interactivity story grows; not adding it today doesn't preclude adding it later.
- **React or Svelte SPA.** Better for richer interactions; we don't have richer interactions to support. Adds a frontend toolchain to the project (build step, bundler, type checker for the frontend, separate dev server). Rejected as overkill for v1.
- **Server-rendered with full-page reloads only (no JS at all).** Simpler still. Rejected because the live-journal feed is meaningfully more useful than a polling refresh, and `EventSource` + a few lines of vanilla JS is a small price.
- **No UI at all.** Considered briefly. Rejected — the inspection use case is real, the demoability matters for OSS adoption, and engineers occasionally need to look at things by hand.

**Implementation notes:**

- The UI is served at `/__ui/` as a tree of routes. `/__ui/groups`, `/__ui/groups/{group}`, `/__ui/routes/{group}/{n}`, etc.
- Static assets (the CSS file, the Ace editor bundle, the small `wm-ace.js` bootstrap) are served at `/__ui/static/*`, embedded in the binary at compile time.
- The UI uses the same session cookie as the rest of the auth system. Bearer-token auth is not used for UI requests; the UI is browser-only.
- minijinja's auto-escape is on for `.html` templates by default — variable interpolation is HTML-safe unless explicitly marked otherwise.
- A `GET /` request that doesn't match a user-created route is redirected to `/__ui/` (authenticated) or `/__auth/login` (not authenticated). See ../route-model.md's "Reserved paths" section.

See also: ../web-ui-design.md, [0010-oauth-oidc.md](0010-oauth-oidc.md), [0011-route-ownership.md](0011-route-ownership.md).
