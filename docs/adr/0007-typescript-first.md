# ADR-0007: TypeScript as the first scripting language

**Status:** Accepted

**Context:** WireMirage is multi-language by design ([0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md)), but the first language has to be picked deliberately. The first language validates the architecture end-to-end, sets the patterns other languages will follow, and (crucially for this project) determines what an LLM agent will be writing handlers in initially.

Three serious candidates: TypeScript, Python, Lua.

**Decision:** TypeScript first, via Javy with Component Model support. Python second, when the architecture has been validated and the WIT contract has stabilized.

**Consequences:**

- **High LLM fluency.** TypeScript has enormous training-data presence; Claude writes idiomatic TS reliably. This matters because the agent will be writing most of the handlers.
- **Mature toolchain.** Javy is the most polished JS-to-Wasm path, recently shipped Component Model support. `tsc` for type checking, `esbuild` for transpilation, both stable and fast.
- **Type checking via `.d.ts`.** The host ships a `.d.ts` describing the script API (request, response, store, log) generated from the WIT file. Editors pick it up automatically; agents see it in context and write correctly-typed handlers against it. Type checking happens in the compiler sidecar's `tsc` step, before Wasm production. Bad code never gets loaded.
- **Cost: TypeScript-as-script is mildly verbose.** Imports, exports, type annotations. For 30-line handlers, this is fine. For one-liner mocks, it's noise. We accept the verbosity; the LLM doesn't care.
- **Cost: it's not Python.** The author is more fluent in Python; some teammates are too. But picking based on author preference ignores the LLM-fluency argument and the toolchain-maturity argument, both of which favor TypeScript. Python comes second, and the gap should close.

**On Lua specifically:** Lua via `mlua` would be the simplest embedding story in any Rust ecosystem, and Lua is genuinely a nice scripting language. Rejected as the first choice because: (a) LLM fluency for Lua is meaningfully lower than for TS or Python, (b) Luau (the typed dialect) has even less training data than vanilla Lua, (c) the embedding story doesn't generalize — picking Lua first means we either skip the Wasm path or add Lua-on-Wasm as a second integration. Lua might still be added later as a "scripting feels heavy, give me a one-line handler" option, but it's not the default.

**On Python specifically:** Python is the right second language because (a) high LLM fluency, (b) the author and many teammates are fluent in it, (c) `componentize-py` is real and works. Going Python second validates that the WIT contract is genuinely language-agnostic — if something in the API doesn't translate cleanly to Python idioms, it's better to find that out with two languages than with five. The expectation is that adding Python will surface 1-3 small issues with the WIT contract, those get fixed as part of v0.2, and then additional languages are mostly mechanical.

**Alternatives considered:**

- **Python first.** Considered seriously, especially given author preference. Rejected because the JS/TS toolchain is more polished, Javy is more battle-tested than `componentize-py`, and starting with the easier integration de-risks the architecture before tackling the harder one. Reversing the order would mean fighting toolchain bugs while also validating architecture decisions.
- **JS without TS.** Lighter pipeline (no `tsc`, no `esbuild`), but loses type checking. Type checking is the thing that catches LLM-generated handlers' mistakes before they hit the runtime, and that feedback loop is genuinely important. Optional alternative: support JSDoc-annotated JS as a lighter on-ramp; consider as a v0.2 nice-to-have.
- **TS plus JS plus JSDoc-JS all from day 1.** Three-way support adds complexity without proportional benefit. Better to pick one canonical input format and add others if there's demand.

See also: [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md), ../script-api-wit.md.
