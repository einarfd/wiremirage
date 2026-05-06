// Server tests: drive the Hono app via its `fetch` handler — no socket
// binding, fast. Exercises the JSON request/response shape and error
// envelopes.

import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { buildApp } from "../src/app.js";

const app = buildApp({
  witPath: resolve(__dirname, "..", "..", "..", "wit"),
  toolchainVersion: "test",
});

const TS_HANDLER = `
export function handle(req, _route, _group) {
  const body = new TextEncoder().encode(\`hi: \${req.method} \${req.path}\`);
  return { status: 200, headers: [["content-type", "text/plain"]], body };
}
`;

async function postJson(path: string, body: unknown): Promise<{ status: number; body: unknown }> {
  const res = await app.fetch(
    new Request(`http://localhost${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
  );
  return { status: res.status, body: await res.json() };
}

describe("server", () => {
  it("GET /health reports liveness", async () => {
    const res = await app.fetch(new Request("http://localhost/health"));
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.status).toBe("ok");
    expect(body.language).toBe("typescript");
    expect(body.bindings_version).toBe("0.1.0");
  });

  it("POST /compile rejects non-string source", async () => {
    const { status, body } = await postJson("/compile", { language: "typescript" });
    expect(status).toBe(400);
    expect((body as { error?: { code?: string } }).error?.code).toBe("invalid_request");
  });

  it("POST /compile rejects unknown language", async () => {
    const { status, body } = await postJson("/compile", {
      language: "ruby",
      source: "puts 'hi'",
    });
    expect(status).toBe(400);
    expect((body as { error?: { code?: string } }).error?.code).toBe("unsupported_language");
  });

  it("POST /compile returns base64 component for a valid TS source", async () => {
    const { status, body } = await postJson("/compile", {
      language: "typescript",
      source: TS_HANDLER,
    });
    expect(status).toBe(200);
    const ok = body as { compiled_wasm?: string; bindings_version?: string };
    expect(typeof ok.compiled_wasm).toBe("string");
    expect(ok.compiled_wasm!.length).toBeGreaterThan(1000);
    expect(ok.bindings_version).toBe("0.1.0");
    // Decode and check the wasm magic bytes.
    const bytes = Buffer.from(ok.compiled_wasm!, "base64");
    expect(bytes[0]).toBe(0x00);
    expect(bytes[1]).toBe(0x61);
    expect(bytes[2]).toBe(0x73);
    expect(bytes[3]).toBe(0x6d);
  }, 60_000);

  it("POST /compile returns compile_failed with diagnostics on bad source", async () => {
    const { status, body } = await postJson("/compile", {
      language: "typescript",
      source: "this is not valid typescript syntax {{{",
    });
    expect(status).toBe(400);
    const err = (body as { error?: { code?: string; diagnostics?: string[] } }).error;
    expect(err?.code).toBe("compile_failed");
    expect(Array.isArray(err?.diagnostics)).toBe(true);
  });
});
