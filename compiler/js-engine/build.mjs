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

import { componentize } from "@bytecodealliance/componentize-js";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
// Pinned to typescript 6 deliberately. 7.x is the Go port, and it does
// not expose the enum surface this file uses — `ts.ScriptTarget` is
// undefined there, so `transpileModule` below dies with "Cannot read
// properties of undefined (reading 'ES2023')". Tried 2026-08; revisit
// when the port's programmatic API settles. There is nothing to gain in
// the meantime: this transpiles one small file at build time, so the
// port's speed is irrelevant here.
import ts from "typescript";

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC = resolve(HERE, "src/engine.ts");
const WIT = resolve(HERE, "wit");

if (!process.env.WM_JS_ENGINE_OUT) {
  console.error(
    "WM_JS_ENGINE_OUT is required (absolute path to the .wasm output).",
  );
  process.exit(1);
}
const OUT = resolve(process.env.WM_JS_ENGINE_OUT);
const OUT_DIR = dirname(OUT);

// 1. Transpile TS → JS. ESNext module shape — componentize-js wants
//    a real ES module, not script-shape. The `export function handle`
//    is what componentize-js looks for to satisfy the world's export.
const tsSrc = readFileSync(SRC, "utf8");
const { outputText, diagnostics } = ts.transpileModule(tsSrc, {
  compilerOptions: {
    target: ts.ScriptTarget.ES2023,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    strict: false,
  },
  reportDiagnostics: true,
});
if (diagnostics && diagnostics.length > 0) {
  const messages = diagnostics.map((d) =>
    typeof d.messageText === "string"
      ? d.messageText
      : d.messageText.messageText,
  );
  console.error("engine.ts transpile failed:");
  for (const m of messages) console.error("  -", m);
  process.exit(1);
}

// 2. componentize-js needs a file on disk. Write into OUT_DIR so the
//    container's bind-mount is writable; /app and /app/src are owned
//    by root (from the Dockerfile COPY) and the build runs as the
//    host UID, so writing inside /app would EACCES.
mkdirSync(OUT_DIR, { recursive: true });
const jsPath = resolve(OUT_DIR, "engine.generated.js");
writeFileSync(jsPath, outputText);

// Optional: pass `--debug` to dump generated bindings to ./debug/.
const DEBUG = process.argv.includes("--debug");
const debugDir = resolve(HERE, "debug");
if (DEBUG) {
  mkdirSync(debugDir, { recursive: true });
}

try {
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
  mkdirSync(OUT_DIR, { recursive: true });
  writeFileSync(OUT, out.component);
  console.log(
    `wrote ${OUT} (${(out.component.length / (1024 * 1024)).toFixed(2)} MiB)`,
  );
} finally {
  // Leave engine.generated.js around for diagnostic purposes; the
  // .gitignore keeps it out of commits.
  void 0;
}
