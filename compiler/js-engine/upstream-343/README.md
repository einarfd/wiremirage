# ComponentizeJS#343 — negative `s64` repro

A standalone reproduction of the upstream defect the engine shim works around:
**lowering a negative `s64` out of JavaScript traps the guest**, before the
value reaches the host.

Upstream: <https://github.com/bytecodealliance/ComponentizeJS/issues/343>
(open since 2026-06-24). Ours: wiremirage#50.

Nothing here is part of the build. It exists so "has upstream fixed it?" is a
two-minute question instead of a rediscovery — **run it after every
componentize-js bump**.

## Status

| | |
|---|---|
| Upstream issue | open, filed 2026-06-24, untouched since (0 comments, unlabelled) |
| Reproduced on | componentize-js 0.20.0 and 0.22.0 (0.22.0 is what we pin) |
| Workaround | shipped, `wrapBucketForNegativeS64` in `../src/engine.ts` |
| User-visible effect | `incr(key, negative)` throws a legible error; negative `listRange` indices work normally |

Everything on our side is done and shipped. Tracking for what we still owe
upstream lives on wiremirage#50, not here.

## Already ruled out

Don't spend time re-deriving these:

- **Not a 0.22 regression.** 0.20.0 behaves identically.
- **Not WireMirage-specific.** The repro here is a three-line guest with no
  host framework, no wasmtime configuration of ours, no engine shim.
- **Not "any 64-bit value with the high bit set".** `sink-u64(2^63)` crosses
  fine. Positive s64 beyond 2^32, negative s32 and negative f64 all pass.
- **Not a lifting problem.** Negatives lift *into* JS correctly in both
  directions — as an export parameter and as an import's return value. Only
  lowering *out* of JS traps.
- **Not fixable in the host.** The guest traps before the import runs, so the
  value never reaches host code; there is nothing host-side to intercept.

## Run

```sh
docker build -t wm-upstream-343 .
mkdir -p out && docker run --rm -v "$PWD/out:/out" --user "$(id -u):$(id -g)" wm-upstream-343
cd runner && cargo run --release -- ../out/guest.wasm
```

## Expected while the defect is present

```
sink-s64(-1)       [lift export param      ] = ok
pull-from-import() [lift import return     ] = ok
source-s64()       [lower export return    ] = TRAP
pull-and-return()  [lift then lower        ] = TRAP
sink-u64(2^63)     [control: bit 63 set    ] = ok
```

The four s64 cases isolate the direction, which is the whole finding:
negatives **lift into** JS fine — both as an export parameter and as an
import's return value — and trap when JS **lowers** one back out, whether
that's an import argument or an export's return. `pull-and-return` traps on
the lowering half, not the lift, which `pull-from-import` proves by doing the
lift alone.

That matters for reading upstream #343: it is written up as *import argument*
lowering, which is one instance of the defect. A fix scoped to that path would
leave export returns broken.

`sink-u64` is the control — a u64 with bit 63 set crosses fine, so this is not
"any 64-bit value with the high bit set". Positive s64 beyond 2^32, and
negative s32 and f64, all pass as well.

## When `source-s64()` returns -3

Upstream is fixed on whatever version `package.json` pins here. Then:

1. Delete `wrapBucketForNegativeS64` and its two call sites in
   `../src/engine.ts`.
2. Delete `negative_incr_delta_reports_the_upstream_limitation` in
   `crates/wm-host/tests/js_engine_dispatch.rs`; **keep**
   `negative_list_range_indices_count_from_the_end` — it asserts the contract's
   semantics, so it is the check that says removal was safe.
3. Drop the `incr` notes from `crates/wm-host/src/capabilities.rs` and
   `docs/handlers.md`.
4. Delete this directory and its entry in the workspace `exclude` list.
5. Close wiremirage#50.

`grep -rn 'ComponentizeJS#343'` finds every touchpoint.
