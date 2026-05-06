import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { CompileError, compile } from "../src/compile.js";

const WIT = resolve(__dirname, "..", "..", "..", "wit");

const TS_HANDLER = `
export function handle(req, _route, _group) {
  const body = new TextEncoder().encode(\`hi: \${req.method} \${req.path}\`);
  return { status: 200, headers: [["content-type", "text/plain"]], body };
}
`;

const JS_HANDLER = `
export function handle(req, _route, _group) {
  const body = new TextEncoder().encode("static");
  return { status: 200, headers: [], body };
}
`;

const BAD_TS = `
export function handle(req, _route, _group {  // missing close paren
  return { status: 200, headers: [], body: new Uint8Array() };
}
`;

const NO_HANDLE_EXPORT = `
export function notHandle() {
  return null;
}
`;

const WASM_MAGIC = new Uint8Array([0x00, 0x61, 0x73, 0x6d]);

function isWasm(bytes: Uint8Array): boolean {
  return WASM_MAGIC.every((b, i) => bytes[i] === b);
}

describe("compile", () => {
  it("componentizes a TypeScript handler", async () => {
    const out = await compile({ language: "typescript", source: TS_HANDLER }, WIT);
    expect(isWasm(out.component)).toBe(true);
    expect(out.bindingsVersion).toBe("0.1.0");
    expect(out.component.length).toBeGreaterThan(1_000_000);
  }, 60_000);

  it("componentizes a JavaScript handler", async () => {
    const out = await compile({ language: "javascript", source: JS_HANDLER }, WIT);
    expect(isWasm(out.component)).toBe(true);
  }, 60_000);

  it("reports TypeScript syntax errors", async () => {
    await expect(
      compile({ language: "typescript", source: BAD_TS }, WIT),
    ).rejects.toThrowError(CompileError);
  });

  it("reports componentize errors when the handler has no `handle` export", async () => {
    await expect(
      compile({ language: "javascript", source: NO_HANDLE_EXPORT }, WIT),
    ).rejects.toThrowError(CompileError);
  }, 60_000);
});
