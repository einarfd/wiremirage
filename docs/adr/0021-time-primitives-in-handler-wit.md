# ADR-0021: Time primitives in the handler WIT contract

**Status:** Accepted

**Context:**

The current `wiremirage:handler` WIT contract gives handlers a request, two storage buckets, and a log interface. It does **not** give them access to wall-clock time, monotonic time, or a `sleep` primitive. componentize-js (the engine bundler behind `js-engine.wasm` per [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md)) explicitly disables the WASI `clocks` feature, so `Date.now()`, `Date()`, and `setTimeout()` all fail at runtime in TS/JS handlers. This was a defensible default at slice-A time — we hadn't seen a concrete use case and "fewer host imports = smaller attack surface" — but the gap blocks real scenarios.

Use cases that motivate adding time primitives:

1. **Latency-induced cascading failure tests.** The forcing function. A user wants to reproduce the failure mode where an upstream API's response time grows over time, eventually crossing a downstream gateway's timeout, at which point the gateway's failover logic (retry, circuit-break, replay) thrashes and amplifies load. Without `sleep`, handlers can't simulate slow responses at all. Without `monotonic`, handlers can't make latency growth a function of elapsed time. This is impossible to reproduce against real upstream APIs because you can't make them slow on demand; a mock that does this is the right tool.

2. **Time-of-day-dependent mocks.** Rate limiters that reset on the hour. Endpoints whose response depends on business hours. APIs that return different data on weekdays vs weekends. All require wall-clock access.

3. **Duration measurement within a handler.** "How long has this group been active?" "When did this user first see route X?" Possible to fake with a counter or a stored timestamp, but only if the handler can read time *now* to compare against the stored value.

4. **Eventual replay of LLM-shaped streaming traffic.** A future streaming-response feature (deferred) needs handlers to emit chunks with controlled inter-chunk delays. That's `sleep` plus per-chunk yield — sleep is half the prerequisite.

The shape of the problem also matters: handler code is short-lived (~30s epoch deadline per [0002-wasm-sandbox.md](0002-wasm-sandbox.md) / slice 46), runs in a sandbox with no I/O other than the host imports we choose to expose, and is written by the operator deploying the mock — not by adversaries. Time primitives that would be unacceptable in an adversarial-handler model (e.g., a public function-as-a-service) are perfectly fine here.

**Decision:**

Add a new `clock` interface to the WIT contract with three host imports:

```wit
interface clock {
  /// Block the handler for `ms` milliseconds before returning. Bounded
  /// by the wasm sandbox's epoch deadline (slice 46, ~30s wall clock);
  /// a sleep longer than that traps the handler. Implementation yields
  /// the host's tokio worker rather than blocking a thread.
  sleep: func(ms: u64);

  /// Current wall-clock time, milliseconds since the Unix epoch (UTC).
  /// May jump backwards on NTP correction; use `monotonic-ms` for
  /// measuring durations.
  wall-time-ms: func() -> u64;

  /// A monotonically non-decreasing counter, in milliseconds. Anchored
  /// at host process start (so wraparound is functionally impossible —
  /// u64 ms is 584 million years). Use this to compute "how much time
  /// has passed since X" reliably: store the value at time T₁, read it
  /// again at T₂, subtract. Never decreases, never jumps backwards;
  /// not tied to any calendar epoch.
  monotonic-ms: func() -> u64;
}
```

The `handler` world imports `clock` alongside `store` and `log`.

The js-engine shim exposes the three as explicit globals on a host-shaped object:

```js
host.sleep(50);           // returns when the host yields back
host.wallTimeMs();        // 1716635420123
host.monotonicMs();       // 4837 (arbitrary anchor, opaque value)
```

Explicit imports, not implicit. We do **not** re-enable componentize-js's `clocks` feature: that would back `Date.now()` natively but also re-enable `setTimeout`, `setInterval`, and the rest of the WASI clocks surface — more surface than we need, and tying the contract to "what JavaScript exposes" makes the WIT less portable to future handler languages (TinyGo, Python). Keeping the three primitives as named host imports is consistent with how `store.*` and `log.*` work today.

The names `wallTimeMs` and `monotonicMs` map kebab-case WIT to camelCase JS per componentize-js convention.

**Consequences:**

- **Cascading-failure tests become writeable.** Handler reads `monotonic-ms` at first request, stores it in `route-store`, computes elapsed-since-first-request on every subsequent request, applies a growing delay via `sleep`. The exact failure mode the motivating user wants to test.
- **Time-of-day and duration-based mocks become writeable.** Rate limiters, business-hours behaviour, "last-modified" headers all work naturally.
- **Sleep cooperates with the existing sandbox bounds.** The slice-46 epoch deadline (~30s) already caps the wall-clock runtime of any single handler invocation. A `sleep(60_000)` traps via epoch interruption, not via any new bound we add here — one less knob.
- **No new memory or storage cost.** Time imports are pure functions; no state stored, no Valkey writes, no journal interaction.
- **Async dispatch already supports this.** wasmtime + tokio's async wasmtime support means `sleep` yields the worker; we don't need to add async machinery, we use what's there.
- **Monotonic semantics are explicit.** `monotonic-ms`'s anchor (process start) is documented as opaque — handlers must only use it for *differences*, never as a calendar time. The "anchored at process start" detail leaks if a handler stores it across a wm-host restart and is then confused; the docstring says "use it to compute durations" but doesn't physically prevent the misuse. We accept that; the failure mode is "handler computes a giant or negative delta," not "host crashes."
- **Future TinyGo handlers get the same primitives** without extra work. The WIT interface is language-agnostic; each language's bindgen exposes them in its idiomatic shape.
- **Determinism trade-off.** Time primitives make handlers non-deterministic (the same request can produce different responses depending on when it runs). This is the point — we want handlers to *be* time-dependent. Operators who want deterministic handlers simply don't call the clock imports.

**Alternatives considered:**

- **Re-enable componentize-js's `clocks` feature.** Backs `Date.now()`, `Date()`, `setTimeout`, `setInterval` natively in JS. More idiomatic JS, but couples the contract to a JavaScript-specific surface and expands the WASI imports beyond what we need. Rejected because TinyGo handlers would still need explicit `clock` host imports (Go doesn't ship with `setTimeout`), and we'd end up with two parallel contracts. Better to define one explicit WIT surface that every handler language binds to.
- **Configure delays declaratively on the route record** (e.g., `route.response_delay_ms`). Solves latency simulation without touching the WIT, but only at the route level — can't condition delay on request content, on state, on number of prior requests, or on elapsed time. Walks away from the dynamic-latency-growth scenario the motivating user wants to test. *Also rejected as a convenience-alongside slice*: once `sleep` is in the WIT, the handler-side equivalent is one line (`host.sleep(200);`) and strictly more capable, so a declarative field would be a second mechanism to do the same thing — usually a smell. The interaction between a route-field delay and an in-handler `sleep` would also be ambiguous. Better to have one way to delay (in-handler), even though the declarative version is mildly cuter for the static case.
- **Pure-`sleep`, no `now` or `monotonic`.** Smaller surface, covers latency simulation but not duration measurement or time-of-day-dependent mocks. Rejected because the latter use cases are real and adding `wall-time-ms` / `monotonic-ms` later would mean a second WIT contract bump.
- **Use the WASI preview2 `wall-clock` and `monotonic-clock` interfaces directly.** WASI defines these in the `wasi:clocks` package. We could `use wasi:clocks/wall-clock@0.2.0;` rather than defining our own. Defers to a well-trodden contract, but pulls in WASI preview2's bigger surface (`subscribe`, `Datetime` records with seconds+nanos, etc.) and ties our handler version to a WASI version. Rejected for the WIT-portability reason above and for simplicity: a handler probably wants `now() -> u64 ms`, not a `Datetime { seconds: u64, nanoseconds: u32 }` record it has to take apart. We can revisit if WASI clocks become the universal expectation.
- **No sleep at all; document handlers as having to fake time another way.** The mock-server-without-delay-support gap is real. Other mock servers (mountebank, WireMock, Prism) all support response delays as a first-class feature. Rejected — we'd be the outlier.

**Implementation order:**

1. **Slice 1 — WIT + host + JS shim + tests.**
   - Add the `clock` interface to `wit/wiremirage.wit` and `compiler/js-engine/wit/wiremirage.wit`.
   - Implement the three host imports in `wm-host` against `tokio::time::sleep`, `chrono::Utc::now()`, and `std::time::Instant::now()` anchored to a `OnceLock<Instant>` initialized at process start.
   - Expose them in the js-engine shim (`compiler/js-engine/src/engine.ts`) as `host.sleep`, `host.wallTimeMs`, `host.monotonicMs`.
   - Tier-2 tests: a `latency-handler` fixture that sleeps for a route-store-controlled duration; a `time-handler` fixture that asserts `monotonic-ms` increases between two calls; a journal-side assertion that the dispatch span's duration reflects the sleep.
   - Update `script-api-wit.md` (Arkiv source-of-truth) before the WIT change, mirror to the repo per the existing convention.
   - Update the wiremirage skill (`skill/wiremirage/`) with a "latency simulation" recipe.

2. **Streaming responses.** Separate ADR. Time primitives are a prerequisite — emit-chunk-with-delay needs `sleep` — but the streaming contract change is much larger and deserves its own decision record.

**See also:**

- [0002-wasm-sandbox.md](0002-wasm-sandbox.md) — the epoch deadline that caps `sleep` doesn't need new machinery.
- [0020-shared-wasm-engine-for-interpreted-languages.md](0020-shared-wasm-engine-for-interpreted-languages.md) — explains why componentize-js's `clocks` feature is disabled today, and why this ADR adds the surface explicitly rather than turning that feature back on.
- ../script-api-wit.md — canonical contract; needs updating in lockstep with `wit/wiremirage.wit`.
- ../route-model.md — for the slice-2 `response_delay_ms` field if pursued.
