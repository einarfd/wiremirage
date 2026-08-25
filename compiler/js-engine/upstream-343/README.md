# ComponentizeJS#343 — negative `s64` repro

A standalone reproduction of the upstream defect the engine shim works around.

**Root cause:** ComponentizeJS's coreabi layer represents a core `i64` in JS as
an **unsigned** BigInt in both directions, while jco's glue assumes the
WebAssembly JS API convention (signed). One cause, two symptoms:

- **Lowering** a negative `s64` out of JS **traps** the guest before the host
  import runs.
- **Lifting** a negative `s64` into JS **does not trap** — it silently yields
  the value plus 2⁶⁴.

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
| Workaround | shipped, `wrapBucketForNegativeS64` in `../src/engine.ts` — corrects both directions |
| User-visible effect | `incr(key, negative)` throws a legible error; negative `listRange` indices and negative counters work normally |

Everything on our side is done and shipped. Tracking for what we still owe
upstream lives on wiremirage#50, not here.

## Already ruled out

Don't spend time re-deriving these:

- **Not a 0.22 regression.** 0.20.0 behaves identically.
- **Not WireMirage-specific.** The repro here is a handful of one-line guest
  functions with no host framework, no wasmtime configuration of ours, and no
  engine shim.
- **Not "any 64-bit value with the high bit set".** `sink-u64(2^63)` crosses
  fine. Positive s64 beyond 2^32, negative s32 and negative f64 all pass.
- **Not fixable in the host.** On the lowering side the guest traps before the
  import runs, so the value never reaches host code.

> **Corrected 2026-08-25.** This list previously claimed "not a lifting
> problem." That was wrong, and *how* it was wrong is worth remembering: the
> guest assigned lifted values to a variable it never read, so the runner could
> only observe *absence of a trap* — which got recorded as "ok". Lifting is
> affected too; it just fails quietly. `lift-matches` below checks the value.

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
lift-matches()     [lift import VALUE      ] = WRONG VALUE (lifted as unsigned)
sink-u64(2^63)     [control: bit 63 set    ] = ok
```

(The runner also interleaves `[host returning -7 to the guest]` notices from
the import; they're elided above.)

Read the cases in pairs. The two `ok` lifts establish only that lifting doesn't
*trap*; `lift-matches` then shows the value is wrong anyway — it compares
`giveSigned() === -7n` inside the guest and returns a `bool`, which travels as
an i32 and is therefore unaffected by the lowering half. The two `TRAP`s are
lowering, and `pull-and-return` traps on the lowering half rather than the
lift, which `pull-from-import` proves by doing the lift alone.

That matters for reading upstream #343: it is written up as *import argument*
lowering, which is one instance of one half of the defect. A fix scoped to that
path would leave export returns and every lift still broken.

`sink-u64` is the control — a u64 with bit 63 set crosses fine, so this is not
"any 64-bit value with the high bit set". Positive s64 beyond 2^32, and
negative s32 and f64, all pass as well.

## When the repro is fully green

Upstream is fixed on whatever version `package.json` pins here. **Green means
both** `source-s64()` returning `-3` *and* `lift-matches()` reporting `ok`. The
lowering fix is the smaller one and the only one #343 actually asks for, so it
may well land alone — don't retire the workaround on a half fix.

Then:

1. Delete `wrapBucketForNegativeS64` and its two call sites in
   `../src/engine.ts`. Both corrections come out together.
2. Delete `negative_incr_delta_reports_the_upstream_limitation` in
   `crates/wm-host/tests/js_engine_dispatch.rs`; **keep**
   `negative_list_range_indices_count_from_the_end` and
   `incr_on_a_negative_counter_returns_the_true_value` — they assert the
   contract's semantics, so they are the checks that say removal was safe.
3. Drop the `incr` notes from `crates/wm-host/src/capabilities.rs` and
   `docs/handlers.md`.
4. Delete this directory and its entry in the workspace `exclude` list.
5. Close wiremirage#50.

`grep -rn 'ComponentizeJS#343'` finds every touchpoint.
