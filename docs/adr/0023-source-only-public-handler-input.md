# ADR-0023: Source-only public handler input (drop pre-compiled-wasm upload)

**Status:** Accepted

**Context:**

The public route-creation surface accepts a handler artifact in one of two shapes: **source** (`language: "typescript" | "javascript"` + `source`), or a **pre-compiled component** (`language: "wasm"` + base64 `compiled_wasm` + `bindings_version`). This is true across REST (`POST /__api/routes`, `PATCH /__api/routes/{g}/{n}`), the CLI (`--wasm-file` / `--bindings-version` alongside `--source-file`), and MCP (`compiled_wasm_b64` alongside `source`).

The pre-compiled path is a holdover from before in-host compilation existed. When [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md) and [0008-handlers-in-storage.md](0008-handlers-in-storage.md) were written, the route artifact *was* a compiled component — uploading wasm was the only way to register a handler. [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md) (slices 56–58) changed that: TS/JS handlers now upload as source, transpile in-host (swc), and dispatch through the shared `js-engine.wasm`. Source became the primary, agent-facing path; pre-compiled wasm became a second, rarely-trodden one.

Two facts make the pre-compiled path hard to justify keeping on the public surface:

1. **The roadmap is source-in for every language.** TS/JS go through the in-host shared engine. The planned second language, TinyGo (and any future AOT language like Rust), would come via a **compile sidecar** — still source-in, compiled host-side, just with a different compiler than the embedded engine. No language on the roadmap requires the user to bring a pre-built component. The pre-compiled-wasm upload therefore serves only "bring an arbitrary component WireMirage has no compiler for" — a power-user escape hatch that sits against the grain of the agent-native premise (agents write source, not binaries; see ../index.md).

2. **It is the only public surface exposed to handler-WIT-contract breaks.** Source handlers are insulated from WIT changes by the engine shim (the contract is host↔`js-engine.wasm`, not host↔source). A pre-compiled component, by contrast, is built against a specific `handler@x.y.z` world and stops instantiating when that world changes. [0022-streaming-http-responses.md](0022-streaming-http-responses.md) is the forcing function: it reshapes the `handle` export (0.1.0 → 0.2.0). Keeping pre-compiled upload means 0022 must build `bindings_version` rejection-gating plus a user-facing migration story; dropping it makes that work unnecessary and shrinks 0022's contract-break blast radius to zero on the public surface.

The project is pre-release with no external users, so narrowing the surface now is free.

**Decision:**

Remove pre-compiled-wasm upload from the **public** surface. Source + language becomes the only public handler input. The **internal** capability to run a route as a wasm component is unchanged.

Concretely:

- **REST.** `CreateRouteBody` / `UpdateRouteBody` drop `compiled_wasm` and the user-supplied `bindings_version`. `language: "wasm"` is no longer an accepted artifact language. The "send either `source` or `compiled_wasm`" branch collapses to "source required" (on create) / "source optional" (on patch). The `SUPPORTED_BINDINGS_VERSION` check moves out of the request path.
- **CLI.** `wm routes add` / `wm routes update` drop `--wasm-file` and `--bindings-version`. `--source-file` is the sole artifact input.
- **MCP.** `create_route` / `update_route` drop `compiled_wasm_b64`. `source` + `language` only — matching the carve-out direction MCP already took before slice 42 re-added source.
- **wm-core.** `CreateRouteRequest` / `UpdateRouteRequest` drop `compiled_wasm` and `bindings_version`.

What stays:

- **The internal component runtime.** Routes still *execute* as wasm components: the shared `js-engine.wasm` for interpreted languages, and (when AOT languages land) a per-route sidecar-compiled component. The registry's internal `NewRoute { language, compiled_wasm, … }` constructor and the `RouteTable` component cache are unchanged — they're how the engine runs, how a future sidecar's output is stored, and how the tier-2 fixtures (standalone Rust crates compiled to components) are loaded. Fixtures load via the internal registry API, not the public REST endpoint, so they are unaffected.
- **An internal bindings-version stamp**, if the runtime still needs one to reason about which world a stored component targets. It is no longer user-supplied or user-visible as an input; whether it remains on the route record as an internal field is an implementation detail for the slice.

This is a *narrowing* of [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md) / [0008-handlers-in-storage.md](0008-handlers-in-storage.md) / [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md)'s public input surface, not a supersession — the internal execution model from those ADRs is intact. AOT languages remain on the table; they arrive as source + sidecar, not as user-uploaded components.

**Consequences:**

- **ADR-0022's contract break has zero public blast radius.** With no user-supplied components, there's nothing to version-gate and no migration story. 0022 slice 1 drops from "reshape `handle` + build the version gate + write the migration" to just "reshape `handle`." (0022 carries a dependency note to this effect.)
- **Smaller, more coherent public surface.** One artifact-input shape (`source` + `language`) across REST / CLI / MCP, matching the product's "handlers are code" premise. Three APIs each lose a branch.
- **Loss: bring-your-own-arbitrary-component.** A user wanting a language WireMirage will never build a compiler for can no longer register a handler. Accepted: no roadmap language needs it, and the use case is speculative and off-premise. **Reversible** — the internal component path stays, so re-exposing a public upload later (with proper world-validation) is additive if a real need appears.
- **Dead-branch cleanup.** `language: "wasm"` retires from public validation. The slice-36 `source: None` case and the slice-37 route-detail "uploaded as pre-compiled `{language}` component" UI branch + `show_route_source`'s "(no source stored …)" message become unreachable for public routes; they are removed or scoped to internal-only routes per the no-dead-scaffolding convention.
- **Docs + skill sweep.** REST (`rest-api.md`), CLI (`cli-design.md`), MCP (`mcp-surface.md`), `route-model.md`, the README's route-creation section, and `skill/wiremirage/` (already source-first, but any wasm-upload mention) update in lockstep.
- **Test adjustment.** Any REST-level test that uploads wasm through the public endpoint moves to the internal registry API or is dropped; fixture-driven dispatch tests already use the internal API and are unaffected.

**Alternatives considered:**

- **Keep pre-compiled upload (status quo).** Low marginal cost to keep, preserves BYO-component. Rejected: it serves no roadmap language, is the sole public path exposed to WIT-contract breaks, and the capability it preserves is speculative — keeping it would be carrying surface for an edge case unrelated to how the product is actually used or planned. It would also force building the `bindings_version` rejection gate for ADR-0022.
- **Deprecate but leave it in (hidden flag / "advanced" docs).** A half-measure: the code still spans three surfaces and still breaks on WIT changes, just less discoverably. Rejected — a decorative-deprecation shim is exactly the kind of scaffolding the repo conventions say to delete rather than keep.
- **Fold this into ADR-0022.** Rejected: it's a distinct decision (handler *input surface*, not streaming), and keeping it separate lets it land first and de-risk 0022. Bundling would muddy both records.
- **Invest in a proper BYO-component story** (validate a user component's imports/exports against the active world, real version negotiation, a stable public ABI). Over-investment in a speculative capability with no current demand. Rejected for now; revisit only if a concrete user need surfaces.

**Implementation order:**

1. **Slice 1 — remove the public input.** Drop `compiled_wasm` / `bindings_version` / `language: "wasm"` from REST bodies, wm-core request models, the CLI flags, and the MCP tool args. `create_route_core` / `patch_route_core` require `source` + a source `language`. Replace the now-removed validation with a clear error if a caller still sends a wasm artifact (e.g. `language: "wasm"` → "pre-compiled wasm upload is no longer supported; send `source` with `language: typescript|javascript`"). Move fixture-using REST tests to the internal registry API as needed.
2. **Slice 2 — cleanup + docs.** Remove the unreachable `source: None` / pre-compiled-wasm UI and `show_route_source` branches (or scope to internal routes). Doc + skill sweep across `rest-api.md`, `cli-design.md`, `mcp-surface.md`, `route-model.md`, README, and `skill/wiremirage/`.

Land both before implementing [0022-streaming-http-responses.md](0022-streaming-http-responses.md).

**See also:**

- [0022-streaming-http-responses.md](0022-streaming-http-responses.md) — the forcing function; with this ADR landed first, 0022's `handle` reshape needs no version gate or migration path.
- [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md) — AOT languages still planned via a compile sidecar (source-in), not user-uploaded components.
- [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md) — established source-in for interpreted languages; this ADR makes source-in the *only* public input.
- [0008-handlers-in-storage.md](0008-handlers-in-storage.md) — the artifact stored on the route record; for public routes it is now always source.
- ../route-model.md, ../rest-api.md, ../cli-design.md, ../mcp-surface.md — surfaces to update in lockstep.
