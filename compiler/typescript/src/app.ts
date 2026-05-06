// Hono app construction. Kept separate from `server.ts` so tests can drive
// the app via `app.fetch(Request)` without binding a port.

import { Hono } from "hono";
import { Buffer } from "node:buffer";
import { BINDINGS_VERSION, CompileError, compile } from "./compile.js";

export interface AppConfig {
  /** Filesystem path the WIT directory; passed verbatim to componentize-js. */
  witPath: string;
  /** Reported in /health. */
  toolchainVersion: string;
}

export function buildApp(config: AppConfig): Hono {
  const app = new Hono();

  app.get("/health", (c) =>
    c.json({
      status: "ok",
      language: "typescript",
      toolchain_version: config.toolchainVersion,
      bindings_version: BINDINGS_VERSION,
    }),
  );

  app.post("/compile", async (c) => {
    let body: unknown;
    try {
      body = await c.req.json();
    } catch (err) {
      return c.json(
        errorBody("invalid_request", `JSON body parse failed: ${describe(err)}`),
        400,
      );
    }

    const language = (body as { language?: unknown }).language;
    const source = (body as { source?: unknown }).source;
    if (typeof source !== "string") {
      return c.json(errorBody("invalid_request", "`source` must be a string"), 400);
    }
    if (language !== "typescript" && language !== "javascript") {
      return c.json(
        errorBody(
          "unsupported_language",
          `language ${JSON.stringify(language)} not supported by this sidecar`,
        ),
        400,
      );
    }

    try {
      const result = await compile({ language, source }, config.witPath);
      return c.json({
        compiled_wasm: Buffer.from(result.component).toString("base64"),
        bindings_version: result.bindingsVersion,
      });
    } catch (err) {
      if (err instanceof CompileError) {
        return c.json(
          {
            error: {
              code: "compile_failed",
              message: err.message,
              diagnostics: err.diagnostics,
            },
          },
          400,
        );
      }
      return c.json(errorBody("internal_error", describe(err)), 500);
    }
  });

  return app;
}

function errorBody(code: string, message: string) {
  return { error: { code, message } };
}

function describe(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
