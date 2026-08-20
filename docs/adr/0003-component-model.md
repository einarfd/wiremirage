# ADR-0003: Wasm Component Model and WIT for the script API

**Status:** Accepted

**Context:** Given that handlers run as Wasm ([0002-wasm-sandbox.md](0002-wasm-sandbox.md)) and the project intends to support multiple scripting languages over time ([0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md)), some contract has to define what host functions handlers can call and what shape `handle` must take. Three layers were possible:

1. **Hand-rolled C ABI** with manually-written language-specific bindings.
2. **Raw WASI** (preview1 or preview2) with conventions on top.
3. **The Component Model + WIT (Wasm Interface Types).**

**Decision:** Use the Component Model with a WIT-defined interface. The contract lives in ../script-api-wit.md.

**Consequences:**

- **One source of truth.** The WIT file defines the API once. Each language's toolchain (`wit-bindgen` for Rust, Javy for JS/TS, `componentize-py` for Python) generates correct bindings from it. The host implements the imports once; handlers in any language consume the same exports.
- **High-level types cross the boundary.** Strings, lists, records, options, and variants are first-class in WIT. Without the Component Model, we'd be hand-marshalling byte arrays for every non-primitive type, which is the kind of thing you get wrong and don't notice until you're debugging UTF-8 corruption in production.
- **Versioning is built in.** WIT packages declare a version (`wiremirage:handler@0.1.0`); compilers report a `bindings_version` to the host; the host can refuse Wasm built against an incompatible API. We get a real evolution story rather than "hope nothing breaks."
- **Some target languages are still rough.** Python via `componentize-py` works but has edges. Go via TinyGo is improving but not all features land cleanly. JS via Javy with component support is good but not as polished as raw WASI Javy. We accept being early adopters here; the Bytecode Alliance is moving fast and most gaps close within a release cycle or two.
- **Tooling lock-in.** We're betting on the Bytecode Alliance ecosystem (`wasmtime`, `wit-bindgen`, `componentize-*`). This is a reasonable bet — it's the canonical Wasm host stack and has serious institutional backing — but it's still a bet.

**Alternatives considered:**

- **Hand-rolled C ABI.** Maximum control, maximum effort. Every language SDK becomes its own integration project. Re-implements what the Component Model gives us free. Rejected.
- **Raw WASI with conventions.** Workable today; Javy initially shipped this way. But everyone using it is migrating to the Component Model, and being one release behind on the migration would be a continuous tax. Going straight to components is cheaper long-term.
- **Don't define a contract; let each language do its own thing.** Catastrophic for multi-language support. Rejected without much consideration.

See also: ../script-api-wit.md, [0004-multi-language-via-sidecars.md](0004-multi-language-via-sidecars.md).
