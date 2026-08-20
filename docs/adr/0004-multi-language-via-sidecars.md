# ADR-0004: Compiler-as-sidecar architecture

**Status:** Accepted

**Context:** Handlers are written in TypeScript (and eventually other languages); they run as Wasm. Something has to compile source to Wasm. The compilation pipeline involves multiple tools — for TypeScript: `tsc` for type checking, `esbuild` for transpilation, `javy` for Wasm production. The host is in Rust.

Three options for where the compiler lives:

1. **In-process inside the Rust host** (e.g., embedding `swc` as a Rust crate for TS-to-JS, plus a Rust binding to Javy).
2. **As a subprocess invoked by the host** (the host shells out to `node`, `tsc`, `esbuild`, `javy`).
3. **As a separate container** (the host calls a sidecar over HTTP or unix socket).

**Decision:** Compiler runs as a separate container, called via HTTP, when the host needs to compile a handler. One container per supported scripting language.

**Consequences:**

- **Adding a new scripting language is "ship another sidecar."** This is the headline benefit. A Python handler container would bundle CPython + `componentize-py`. A Lua container would bundle the Lua-to-Wasm toolchain. Same HTTP shape, same versioning protocol, the host doesn't change. This is more valuable than it looks: it's the difference between "TypeScript-only forever" and "polyglot mock platform."
- **The Rust host stays small and focused.** No bundled TypeScript compiler, no Python embedding, no language-specific build code. The host does routing, execution, storage, admin/MCP. Compilers do compilation. Clean responsibility split.
- **Deployment is two containers instead of one.** Acceptable given the project's deployment targets (Docker Compose for dev, K8s for production-ish use). Worth zero marginal complexity for users; some marginal complexity for the project's CI.
- **Network hop for compilation.** Loopback or in-cluster, so a few ms — negligible compared to actual compilation time. Compiled Wasm is cached, so steady-state has no compile cost at all.
- **Image size for the compiler container.** A Node + `tsc` + `esbuild` + `javy` image is in the 200-400MB range. Doesn't matter on a server. Slightly annoying for local pull. A Bun-based variant would be smaller and is worth investigating.
- **Local-only mode without containers is harder.** A developer running just the Rust binary can't compile new TypeScript handlers without the compiler container also running. We need a fallback: either accept pre-compiled `.wasm` directly (always allowed), or ship a "compiler shim" the host can invoke as a subprocess for single-binary deployment.

**Alternatives considered:**

- **Embed `swc` as a Rust crate for in-process TS-to-JS.** Pure-Rust, no subprocess, no container. Considered seriously. Rejected because it doesn't generalize to Python, Lua, etc. — every new language would still be a major lift, and we'd have inconsistent integration patterns across languages. The whole point of this decision is to make the Nth language cheap.
- **Bundle Node.js into the host's container so subprocess works without a sidecar.** Simpler for users, but conflates two concerns (host and compiler) into one image, makes the image significantly bigger, and means the Rust host has to know about each language's toolchain. Loses the "add a language by adding a container" property.
- **Run compilers in Wasm too (Javy compiling to Wasm, executed in the host).** Considered. `tsc` itself doesn't run cleanly in QuickJS-via-Javy; the JavaScript engine has gaps `tsc` hits. Workarounds exist for stripper-only paths (sucrase, `@swc/wasm`) but they lose type checking, which is exactly what we want from TypeScript in an agent-driven workflow. Cool idea, wrong tradeoff.

See also: [0001-rust-host.md](0001-rust-host.md), [0007-typescript-first.md](0007-typescript-first.md).
