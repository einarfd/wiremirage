# ADR-0032: Sandbox resource limits for a multi-tenant host — keep the wall-clock bound, make it a per-route budget within a ceiling

**Status:** Accepted

**Amends:** the limit configuration in [0002-wasm-sandbox.md](0002-wasm-sandbox.md) (as implemented in slice 46).

**Context:**

[0002-wasm-sandbox.md](0002-wasm-sandbox.md) (slice 46) gives every handler three independent bounds, whichever fires first traps the call:

- **Fuel** (`consume_fuel`) — caps CPU work; a busy-loop burns fuel and traps.
- **Epoch deadline** (`epoch_interruption`) — a wall-clock budget; buffered handlers get the short budget (~30s), streaming handlers a longer one (~5 min).
- **Memory** — a 64 MiB linear-memory cap.

`ResourceUsage` captures `fuel_consumed`, `memory_peak_bytes`, `wall_clock_ms` per call.

The move to virtual-host multi-tenancy ([0030-virtual-host-routing.md](0030-virtual-host-routing.md)) raised the question: with a "wider" per-tenant route surface, do the **wall-clock** limits still make sense, or should the host meter only **CPU (fuel)** and stop caring about time?

The load-bearing technical fact: **fuel meters executed guest instructions only.** Time a handler spends *not executing* — `host.sleep` ([0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md)), awaiting, or blocked on stream backpressure — burns ~0 fuel. So fuel alone cannot bound a handler that is deliberately slow, hung, or buggy-waiting; **only a wall-clock deadline can.** And two pressures pull in opposite directions:

- Mocks *legitimately* simulate latency, slow upstreams, and long-lived SSE / streaming-LLM responses. The fixed buffered-~30s cap is already friction — the documented way to simulate an upstream that hangs past 30s is "use a streaming handler" purely to get the longer budget, even when you don't actually want to stream.
- On a **shared** host, an unbounded handler holds a wasm instance *and* a client connection; one tenant's hung or pathologically-slow handler can starve shared capacity. Per-handler bounds therefore matter **more** in multi-tenant, not less.

**Decision:**

**Keep both fuel and a wall-clock deadline — do not move to CPU-only.** Dropping the wall-clock bound in favour of fuel would leave sleeping/waiting/backpressured handlers unbounded, which fuel structurally cannot catch and which is a cross-tenant resource-starvation vector.

Reshape the wall-clock bound for the wider world:

- The wall-clock budget becomes a **per-route (optionally per-group) configurable value within a hard ceiling**, defaulting to the current modest value. A slow-LLM / SSE / deliberate-hang mock opts into a longer budget *explicitly*, up to the ceiling (≈ today's streaming bound). This **realises route-model.md's "per-route resource limit overrides" future item**, now with a concrete driver.
- This **unifies** the buffered-vs-streaming budget split: "how long may I run" is the per-route budget; "do I stream chunks or buffer" stays an orthogonal handler choice. Streaming stops being the backdoor for "I want more wall time."
- Fuel stays the CPU bound (busy-loop protection); memory cap unchanged. The journal already records which limit fired — keep surfacing fuel/epoch/memory as the trap reason.

**Deferred (flagged, not built):** **per-tenant concurrency / rate fairness** — bounding how many concurrent (or how much aggregate) handler time one tenant can consume so they don't starve others. This is a genuinely new axis the flat single-namespace model never needed. It is the right *next* line of defence after per-request bounds, but premature for the current single small deployment with no observed contention. Revisit when contention is actually seen.

**Consequences:**

- Latency / hang simulation gets a **first-class knob** (per-route wall budget) instead of the streaming backdoor — better fidelity for slow-API mocks and the exact thing the first agent user wanted (simulate a connection that hangs past 30s).
- Per-request fairness preserved: every handler is still bounded in CPU, wall-clock, and memory, which is what protects a shared host at the per-call level.
- The buffered/streaming distinction stops being load-bearing for run-time.
- **Cost:** a per-route limit field (storage + validation + plumbing into the per-`Store` fuel/epoch setup), a ceiling constant, and surfacing the field on the create/update/show surfaces. Modest, and independent of the virtual-host epic — it can land before or alongside it.
- **Deferred** per-tenant concurrency is named so it isn't forgotten, without being built speculatively.

**Alternatives considered:**

- **Meter CPU (fuel) only; drop the wall-clock deadline.** Rejected: fuel cannot see `sleep` / await / backpressure, so a slow-or-hung handler burns ~0 fuel and would run unbounded, holding an instance + connection — strictly worse on a shared host. The wall-clock deadline is precisely the bound for "slow but not CPU-heavy."
- **Keep the fixed global 30s / 5 min split.** Rejected: it's already friction (you must stream to exceed 30s even when you don't want to), and a per-route budget within a ceiling is the cleaner generalisation.
- **Build per-tenant concurrency limits now.** Rejected as premature — no observed contention, single small deploy; flagged for later instead.
- **Make the budget per-group only (not per-route).** Reasonable, but per-route is finer-grained and a group-level default can layer on top; chose per-route with optional group default.

**See also:** [0002-wasm-sandbox.md](0002-wasm-sandbox.md) (amended), [0021-time-primitives-in-handler-wit.md](0021-time-primitives-in-handler-wit.md) (`host.sleep` — what fuel can't meter), [0022-streaming-http-responses.md](0022-streaming-http-responses.md) (the buffered/streaming budget split this unifies), [0030-virtual-host-routing.md](0030-virtual-host-routing.md) (the multi-tenant fairness driver), ../route-model.md (the per-route-resource-overrides future item this realises)
