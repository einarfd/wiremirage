// Build the js-engine wasm component (ADR-0020 / slice 56).
//
// Transpiles engine.ts → engine.js (no module-system rewriting; the
// componentize-js Wizer step expects the file to be a real ES
// module), then runs componentize-js with the `engine` world to
// produce js-engine.wasm.
//
// Invoked from the host's `build.rs` via a pinned Docker container
// (see `Dockerfile` in this directory). The cargo `OUT_DIR` is
// bind-mounted at /out and `WM_JS_ENGINE_OUT=/out/js-engine.wasm`
// selects the output path.

// componentize-js -> @bytecodealliance/weval -> decompress (npm) pulls in
// three known zip-slip / arbitrary-file-write advisories (GHSA, dead since
// 2020, unmaintained). Dismissed as tolerable risk 2026-08: decompress only
// runs here, inside this throwaway Docker build stage, to unpack weval's own
// precompiled binary from a fixed github.com/bytecodealliance/weval release
// tag — never attacker- or SUT-controlled input, and never part of the
// shipped image. Upstream already fixed it (bytecodealliance/weval#32,
// decompress -> tar+fflate, merged 2026-07-08) but hasn't cut a release past
// it yet (npm latest is still 0.4.1, pre-fix). Re-check on the next
// componentize-js/weval bump; the advisories should just disappear then.
import { componentize } from "@bytecodealliance/componentize-js";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const WIT = resolve(HERE, "wit");

if (!process.env.WM_JS_ENGINE_OUT) {
  console.error(
    "WM_JS_ENGINE_OUT is required (absolute path to the .wasm output).",
  );
  process.exit(1);
}
if (!process.env.WM_JS_ENGINE_SRC) {
  console.error(
    "WM_JS_ENGINE_SRC is required (absolute path to the transpiled engine JS).\n" +
      "wm-host's build.rs transpiles src/engine.ts with wm-transpile and passes\n" +
      "the result in — see ADR-0038. To run this container by hand, transpile\n" +
      "engine.ts yourself and point WM_JS_ENGINE_SRC at the .js.",
  );
  process.exit(1);
}
const OUT = resolve(process.env.WM_JS_ENGINE_OUT);
const OUT_DIR = dirname(OUT);

// TS → JS already happened, in Rust, using the same swc that transpiles
// user handlers (ADR-0038). This container only componentizes. That is
// why there is no typescript dependency here for emit: `tsc` runs in the
// image as a *checker* (npm run typecheck, wired into the Dockerfile),
// never as the thing producing this file.
const jsPath = resolve(process.env.WM_JS_ENGINE_SRC);
if (!existsSync(jsPath)) {
  console.error(`WM_JS_ENGINE_SRC does not exist: ${jsPath}`);
  process.exit(1);
}
mkdirSync(OUT_DIR, { recursive: true });

// Optional: pass `--debug` to dump generated bindings to ./debug/.
const DEBUG = process.argv.includes("--debug");
const debugDir = resolve(HERE, "debug");
if (DEBUG) {
  mkdirSync(debugDir, { recursive: true });
}

// When the componentize-js pin in package.json moves, re-run the repro in
// ./upstream-343/ before merging: it answers whether ComponentizeJS#343
// (negative s64 lowering traps the guest) is fixed on the new version, and
// its README lists what to delete when it is. `grep -rn 'ComponentizeJS#343'`
// finds the workarounds that depend on the answer.
const out = await componentize({
    sourcePath: jsPath,
    witPath: WIT,
    worldName: "engine",
    // Pass our env through to the spawned wizer/weval subprocess.
    // Without this, componentize-js synthesises a tiny env containing
    // only its own bookkeeping vars (DEBUG, SOURCE_NAME, EXPORT_*),
    // and wizer's wasmtime cache then can't find $HOME — it errors
    // with "config file not specified and failed to get the default"
    // before doing any real work. We rely on the Dockerfile to pin
    // HOME=/tmp so this works regardless of which UID we run as.
    env: process.env,
    // Same restrictions as the per-route handler builds: no real
    // clocks, no real RNG, no fetch, no stdio. The host provides
    // logging via the `log` interface.
    disableFeatures: ["stdio", "random", "clocks", "http", "fetch-event"],
    ...(DEBUG
      ? {
          debugBindings: true,
          debug: {
            bindingsDir: debugDir,
            bindings: true,
          },
        }
      : {}),
});
writeFileSync(OUT, out.component);
console.log(
  `wrote ${OUT} (${(out.component.length / (1024 * 1024)).toFixed(2)} MiB)`,
);
