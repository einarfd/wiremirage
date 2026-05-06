// Compiles a handler source into a Wasm component.
//
// Pipeline:
//   1. (TypeScript only) transpile via tsc.transpileModule → JS source.
//      We don't run the type-checker — handlers are typically small enough
//      that a fast transpile + componentize is more useful as a tight
//      feedback loop. Type errors caught downstream by jco's bindings
//      validation.
//   2. Write the JS to a temp file (componentize-js requires a file path).
//   3. Invoke componentize-js with our WIT directory + world name.
//   4. Return the component bytes.

import { componentize } from "@bytecodealliance/componentize-js";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as ts from "typescript";

export interface CompileRequest {
  language: "typescript" | "javascript";
  source: string;
}

export interface CompileResult {
  component: Uint8Array;
  bindingsVersion: string;
}

export class CompileError extends Error {
  diagnostics: string[];
  constructor(message: string, diagnostics: string[] = []) {
    super(message);
    this.name = "CompileError";
    this.diagnostics = diagnostics;
  }
}

/** Bindings version this sidecar produces. Must match the host's expectation. */
export const BINDINGS_VERSION = "0.1.0";

/**
 * Compile a handler source into a Wasm component against the WIT at
 * `witPath`.
 */
export async function compile(
  request: CompileRequest,
  witPath: string,
): Promise<CompileResult> {
  const js = transpile(request);
  const work = mkdtempSync(join(tmpdir(), "wm-compile-"));
  try {
    const sourceFile = join(work, "handler.js");
    writeFileSync(sourceFile, js);
    const out = await componentize({
      sourcePath: sourceFile,
      witPath,
      worldName: "handler",
      // Mocks are sandboxed: no real clocks, no real RNG, no fetch. The
      // host provides logging via the `log` interface (we don't take a
      // wasi:io dependency for stdio either; it'd break runs against
      // hosts that don't link WASI Preview 2).
      disableFeatures: ["stdio", "random", "clocks", "http", "fetch-event"],
    });
    return { component: out.component, bindingsVersion: BINDINGS_VERSION };
  } catch (err) {
    throw mapComponentizeError(err);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

function transpile(request: CompileRequest): string {
  if (request.language === "javascript") {
    return request.source;
  }
  const { outputText, diagnostics } = ts.transpileModule(request.source, {
    compilerOptions: {
      target: ts.ScriptTarget.ES2023,
      module: ts.ModuleKind.ESNext,
      moduleResolution: ts.ModuleResolutionKind.Bundler,
      strict: false,
      esModuleInterop: true,
    },
    reportDiagnostics: true,
  });
  if (diagnostics && diagnostics.length > 0) {
    const messages = diagnostics.map((d) =>
      typeof d.messageText === "string"
        ? d.messageText
        : d.messageText.messageText,
    );
    // tsc.transpileModule reports parse-level errors here; type errors are
    // not produced because we don't run the type-checker. So any diagnostic
    // is a hard syntax issue and should fail the request.
    throw new CompileError("TypeScript transpile failed", messages);
  }
  return outputText;
}

function mapComponentizeError(err: unknown): CompileError {
  if (err instanceof CompileError) return err;
  const msg = err instanceof Error ? err.message : String(err);
  // componentize-js errors include "JS component error: ..." prefixes; the
  // detail text is what the user wants. We don't try to parse further.
  return new CompileError("componentization failed", [msg]);
}
