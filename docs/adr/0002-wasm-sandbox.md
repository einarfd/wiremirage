# ADR-0002: Wasm via wasmtime for handler execution

**Status:** Accepted

**Context:** Handlers are user-authored scripts that run in response to HTTP requests. Even when the author is on the same team and "trusted," the handler-execution environment needs:

- **Isolation** so a buggy handler can't crash the host or leak state into other handlers.
- **Resource limits** (CPU time, memory, wall clock) so a handler with a runaway loop doesn't take down the server.
- **A clean API surface** that exposes only the host functions the handler is supposed to use — not the full host language's standard library, not the host's filesystem, not the host's network.
- **Multi-language support** so handlers can be written in TypeScript, Python, possibly Go or Lua, without per-language host integration code.

Same-language sandboxing (e.g., Node's `vm` module, Python's restricted execution) was considered and rejected: removing dangerous features from a shared environment is a known minefield, and the per-language sandbox stories aren't comparable across languages.

**Decision:** Execute handlers as WebAssembly modules in `wasmtime`, embedded inside the Rust host.

**Consequences:**

- Strong sandbox by construction. The Wasm module boundary is the isolation boundary; handlers can call only the host functions explicitly imported into their module's instance.
- Resource limits are first-class in `wasmtime` — CPU "fuel" (instruction count), memory cap, instantiation pooling. Per-invocation enforcement.
- Per-request fresh instantiation is cheap enough in wasmtime (sub-millisecond for our handler shape) that we don't reuse instances across requests. Clean state isolation between invocations comes free.
- Multi-language story is unified: any language that compiles to Wasm can be a handler language, with the same isolation properties and resource-limit machinery.
- Cost: handlers must be compiled to Wasm before they can run. This means a build step, which we handle via [compiler sidecars](0004-multi-language-via-sidecars.md). The build step adds latency on first load (typically <1s) but compiled `.wasm` is cached, so steady-state has no compile cost.
- Cost: the Wasm Component Model is still maturing in some target languages (especially Python and Go). We're early-but-not-bleeding-edge. Worth flagging in [0003-component-model.md](0003-component-model.md).

**Alternatives considered:**

- **Native subprocess per handler.** Strong isolation via OS process boundaries, but startup cost is meaningfully higher (10-100ms for a fresh process), and the cross-language story is uglier (each language needs its own runtime image). Better for some workloads, worse for ours.
- **Embedded language interpreter (e.g., `mlua` for Lua, `rustpython` for Python).** Simpler than Wasm, but locks in one language per host. Would force "TypeScript via Lua-like interpreter" or similar awkward fits. Defeats the multi-language goal.
- **No sandbox; trust the author.** Tempting for a single-team tool. Rejected because handler bugs ("`while(true)` in a test mock") would take down the server, and the agent will sometimes write code that does this. The cost of sandboxing is low enough that the safety property is worth keeping.
- **V8 isolates (à la Cloudflare Workers).** Would only support JS-flavored languages. The Component Model gives us the same per-invocation isolation properties with broader language support.

See also: [0001-rust-host.md](0001-rust-host.md), [0003-component-model.md](0003-component-model.md).
