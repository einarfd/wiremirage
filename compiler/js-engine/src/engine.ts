// js-engine shim — the JavaScript-side of the shared wasm engine
// designed in ADR-0020.
//
// componentize-js bundles StarlingMonkey + this file once, ahead of
// time, into `js-engine.wasm`. The host loads that component once at
// startup and instantiates it per request. Per-route artefacts are
// plain JS source strings stored on the `Route` record; the engine
// asks the host for the matched route's source via the
// `wiremirage:engine/engine-host.get-source` import on every call to
// `handle`, evaluates it, and dispatches.
//
// Contract with the user source:
//
//   The source must be a JavaScript *script* (not an ES module) that
//   declares a top-level `function handle(req, route, group)`. The
//   TS→JS transpile step that produces it strips ES `export` keywords
//   so the same shape works whether the operator wrote
//   `export function handle(...)` (TypeScript style) or
//   `function handle(...)` (script style) at the source surface.

// componentize-js generates JS-side bindings for the world's host
// imports under the `wiremirage:engine/<interface>` specifier. The
// type-only `declare module` blocks below make TypeScript happy
// during the build; at runtime the bindings are provided by the host.
declare module "wiremirage:handler/engine-host@0.1.0" {
  /** Return the JS source for the route this request matched. */
  export function getSource(): string;
}

declare module "wiremirage:handler/clock@0.1.0" {
  /** Block the calling handler for `ms` milliseconds (ADR-0021). */
  export function sleep(ms: bigint): void;
  /** Wall-clock milliseconds since the Unix epoch (UTC). */
  export function wallTimeMs(): bigint;
  /** Monotonic milliseconds since host process start; only useful as a difference. */
  export function monotonicMs(): bigint;
}

import { getSource } from "wiremirage:handler/engine-host@0.1.0";
import {
  sleep as clockSleep,
  wallTimeMs as clockWallTimeMs,
  monotonicMs as clockMonotonicMs,
} from "wiremirage:handler/clock@0.1.0";

// Expose the clock primitives to user code as a `host` global. The
// WIT signatures use `u64`, which componentize-js maps to `bigint` on
// the JS side. We wrap with Number coercion so user code can write
// `host.sleep(100)` rather than `host.sleep(100n)` — the same shape
// most operators will reach for. Values beyond 2^53 truncate, which
// for ms-units is 285k years and not a real concern.
(globalThis as Record<string, unknown>).host = {
  sleep: (ms: number | bigint): void => {
    clockSleep(typeof ms === "bigint" ? ms : BigInt(Math.max(0, Math.trunc(ms))));
  },
  wallTimeMs: (): number => Number(clockWallTimeMs()),
  monotonicMs: (): number => Number(clockMonotonicMs()),
};

// Each call to `handle` is a fresh wasmtime instance, so caching the
// compiled user-handle function across requests doesn't pay rent.
// (If we ever move to an instance pool, this is where the cache
// would live.) Today it's parse-and-execute every time.
export function handle(
  req: unknown,
  routeStore: unknown,
  groupStore: unknown,
): unknown {
  const source = getSource();
  // Use the Function constructor rather than eval(): keeps the user's
  // handler in its own closure (no leaking of engine internals via
  // shared lexical scope), and gives us a clean way to return the
  // user-defined `handle` symbol back into our scope.
  //
  // The user source is responsible for declaring `function handle(...)`.
  // Trailing `; return handle;` makes the inner symbol visible to us.
  // Wrap in a try to convert syntax errors into a structured response;
  // wasm-side traps would just kill the request with a 500 otherwise.
  let userHandle: unknown;
  try {
    const factory = new Function(source + "\n;return handle;");
    userHandle = factory();
  } catch (e) {
    return errorResponse(
      500,
      `engine: syntax error in handler source: ${formatError(e)}`,
    );
  }
  if (typeof userHandle !== "function") {
    return errorResponse(
      500,
      "engine: handler source did not declare a top-level `handle` function",
    );
  }
  try {
    return (userHandle as (a: unknown, b: unknown, c: unknown) => unknown)(
      req,
      routeStore,
      groupStore,
    );
  } catch (e) {
    return errorResponse(500, `engine: handler threw: ${formatError(e)}`);
  }
}

function errorResponse(status: number, message: string): unknown {
  return {
    status,
    headers: [["content-type", "text/plain; charset=utf-8"]],
    body: new TextEncoder().encode(message),
  };
}

function formatError(e: unknown): string {
  if (e instanceof Error) {
    return `${e.name}: ${e.message}`;
  }
  try {
    return String(e);
  } catch {
    return "(unstringable error)";
  }
}

