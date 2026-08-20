# ComponentizeJS#343 — negative `s64` repro

A standalone reproduction of the upstream defect the engine shim works around:
**lowering a negative `s64` out of JavaScript traps the guest**, before the
value reaches the host.

Upstream: <https://github.com/bytecodealliance/ComponentizeJS/issues/343>
(open since 2026-06-24). Ours: wiremirage#50.

Nothing here is part of the build. It exists so "has upstream fixed it?" is a
two-minute question instead of a rediscovery — **run it after every
componentize-js bump**.

## Run

```sh
docker build -t wm-upstream-343 .
mkdir -p out && docker run --rm -v "$PWD/out:/out" --user "$(id -u):$(id -g)" wm-upstream-343
cd runner && cargo run --release -- ../out/guest.wasm
```

## Expected while the defect is present

```
sink-s64(-1)    [negative IN, no return ] = ok
source-s64()    [negative OUT, no params] = TRAP
sink-u64(2^63)  [control: bit 63 set    ] = ok
```

`sink-s64` passing and `source-s64` trapping is the whole finding: negatives
*lift into* JS fine and fail when JS *lowers them back out*. The `u64` control
rules out "any 64-bit value with the high bit set". Positive `s64` beyond 2^32,
`s32` and `f64` negatives all pass too — earlier revisions of this repro
covered them.

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
