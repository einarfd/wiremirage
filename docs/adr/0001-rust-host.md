# ADR-0001: Rust for the implementation language

**Status:** Accepted

**Context:** The WireMirage host is a long-running web server with hot request paths, an embedded scripting runtime, an out-of-process storage backend, and a fixed set of well-bounded protocols (HTTP first, possibly WebSocket later). The project's other constraints — sandboxed handler execution via Wasm, an MCP server, a REST API, integration with a Redis-protocol storage layer — all suggest a systems-level language.

The author is fluent in Python and competent in Rust (used in earlier projects). Other realistic options were Go (good ecosystem, simpler than Rust, less precise control) and Python (fastest for the author personally, but a poor fit for the embedded-Wasm story).

**Decision:** Implement the host in Rust.

**Consequences:**

- The mature Rust Wasm ecosystem (`wasmtime`, `wit-bindgen`, the Component Model tooling) is the project's biggest dependency, and it's first-class in Rust. Other languages can host wasmtime via FFI, but it's not as ergonomic.
- `axum` + `tokio` + `tower` covers the HTTP and middleware story. `serde` covers JSON. `redis-rs` covers storage (see [0005-valkey-storage.md](0005-valkey-storage.md)). None of these are surprises.
- Compile times and the borrow checker will slow the first 200 lines compared to Python. Acceptable cost for a project the author has time-budgeted in weekends rather than days.
- Contributor pool for an OSS project is smaller than Go or Python would yield. Mitigated somewhat by the project being narrow-scope and the contracts (the WIT file) being language-agnostic.

**Alternatives considered:**

- **Go.** Simpler to write, larger contributor pool, but the wasmtime-go bindings are less mature than the native Rust ones, and the Wasm Component Model story in Go is rougher than in Rust. Would have been the right choice for "ship fast, broad audience"; the wrong choice for "first-class Component Model integration."
- **Python.** Fastest for the author. But embedding wasmtime via the Python bindings is workable, not idiomatic; the runtime overhead and GIL would matter on the request hot path; and writing a long-running web server in Python that's also doing significant per-request work in a Wasm runtime is fighting the language. Would also be a strange dependency to introduce — running a Python service in front of a Wasm runtime to mock another Python service.
- **TypeScript / Node.** Same script language as the first-supported handler language, so there'd be appeal in unifying. But Node is a poor host for embedded Wasm with strong isolation guarantees, and the perf profile is wrong for a tool that may be running thousands of small requests in tests.

See also: [0002-wasm-sandbox.md](0002-wasm-sandbox.md), [0003-component-model.md](0003-component-model.md).
