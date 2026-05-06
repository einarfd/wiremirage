// Entry point: wires config from env vars, builds the app, binds a port.
// Imports of `app.ts` from tests do not run this file.

import { serve } from "@hono/node-server";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { buildApp } from "./app.js";
import packageJson from "../package.json" with { type: "json" };

const PORT = parseInt(process.env.WM_COMPILER_PORT ?? "9100", 10);
const WIT_PATH = resolve(process.env.WM_HANDLER_WIT_PATH ?? "../../wit");
const TOOLCHAIN_VERSION = packageJson.version || "0.0.0";

if (!existsSync(WIT_PATH)) {
  // eslint-disable-next-line no-console
  console.error(
    `WIT path ${WIT_PATH} does not exist. Set WM_HANDLER_WIT_PATH or run from the sidecar directory with the wit/ folder mounted at ../../wit.`,
  );
  process.exit(1);
}

const app = buildApp({ witPath: WIT_PATH, toolchainVersion: TOOLCHAIN_VERSION });

serve({ fetch: app.fetch, port: PORT, hostname: "0.0.0.0" }, (info) => {
  // eslint-disable-next-line no-console
  console.log(
    `wiremirage compiler-typescript listening on ${info.address}:${info.port} (wit=${WIT_PATH})`,
  );
});
