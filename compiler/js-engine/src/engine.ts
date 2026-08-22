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

// The WIT host imports this shim consumes are declared ambiently in
// `wit-modules.d.ts` — they have to live in a .d.ts, because `declare
// module` inside a module file is an *augmentation* and TypeScript
// rejects augmenting a module it cannot resolve (ADR-0038).

import { getSource } from "wiremirage:handler/engine-host@0.1.0";
import {
  sleep as clockSleep,
  wallTimeMs as clockWallTimeMs,
  monotonicMs as clockMonotonicMs,
} from "wiremirage:handler/clock@0.1.0";
import {
  start as respStart,
  writeChunk as respWriteChunk,
  finish as respFinish,
} from "wiremirage:handler/response-stream@0.1.0";
import { schedule as cbSchedule } from "wiremirage:handler/callback@0.1.0";
import { emit as logEmit } from "wiremirage:handler/log@0.1.0";

// Expose the clock primitives to user code as a `host` global. The
// WIT signatures use `u64`, which componentize-js maps to `bigint` on
// the JS side. We wrap with Number coercion so user code can write
// `host.sleep(100)` rather than `host.sleep(100n)` — the same shape
// most operators will reach for. Values beyond 2^53 truncate, which
// for ms-units is 285k years and not a real concern.
// Coerce a chunk (string | Uint8Array | ArrayBuffer) to bytes for the
// host write-chunk import. Strings are UTF-8 encoded — the common case
// for SSE / text streams.
function toBytes(chunk: unknown): Uint8Array {
  if (chunk instanceof Uint8Array) return chunk;
  if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
  return new TextEncoder().encode(String(chunk));
}

(globalThis as Record<string, unknown>).host = {
  sleep: (ms: number | bigint): void => {
    clockSleep(typeof ms === "bigint" ? ms : BigInt(Math.max(0, Math.trunc(ms))));
  },
  wallTimeMs: (): number => Number(clockWallTimeMs()),
  monotonicMs: (): number => Number(clockMonotonicMs()),
  // ADR-0022 streaming responses. `host.responseStream({status, headers})`
  // commits the head and returns a writer: `.write(chunk)` flushes a
  // chunk (returns false once the client is gone), `.close()` ends the
  // body. A handler that streams doesn't need to return a response.
  responseStream: (init?: {
    status?: number;
    headers?: [string, string][];
  }): { write: (chunk: unknown) => boolean; close: () => void } => {
    respStart(init?.status ?? 200, init?.headers ?? []);
    return {
      write: (chunk: unknown): boolean => respWriteChunk(toBytes(chunk)),
      close: (): void => respFinish(),
    };
  },
  // ADR-0034 outbound callbacks. `host.scheduleCallback({ url, method,
  // headers, body, delayMs })` hands a webhook request to the host, which
  // fires it ONCE after `delayMs`, after this response is sent. Throws
  // synchronously if callbacks aren't enabled (host egress off, or this
  // group hasn't set callout_enabled) so the handler can catch it. The
  // delivery outcome lands in the group's callback journal, not the
  // response (which has already returned). Defaults: POST, no headers,
  // empty body, fire immediately. `body` may be a string (UTF-8 encoded)
  // or bytes.
  scheduleCallback: (init: {
    url: string;
    method?: string;
    headers?: [string, string][];
    body?: unknown;
    delayMs?: number;
  }): void => {
    cbSchedule(
      init.url,
      (init.method ?? "POST").toUpperCase(),
      init.headers ?? [],
      init.body === undefined || init.body === null
        ? new Uint8Array()
        : toBytes(init.body),
      BigInt(Math.max(0, Math.trunc(init.delayMs ?? 0))),
    );
  },
};

// Handler logging. The `log` host import (WIT `interface log`) routes each
// line to this request's journal entry (`handler_logs`), visible via
// `wm journal show`, the journal-entry UI, and `dry_run`. Before this the
// `log` global was never surfaced, so the documented logging path was dead
// and `console.*` (StarlingMonkey's stderr console) reached nothing.
const LOG_LEVELS = ["debug", "info", "warn", "error"] as const;
type LogLevel = (typeof LOG_LEVELS)[number];

function stringifyArg(value: unknown): string {
  if (typeof value === "string") return value;
  try {
    return value !== null && typeof value === "object"
      ? JSON.stringify(value)
      : String(value);
  } catch {
    return String(value);
  }
}

function emitAt(level: LogLevel, args: unknown[]): void {
  logEmit(level, args.map(stringifyArg).join(" "));
}

// Faithful to the WIT `log.emit(level, message)`, plus level-named
// conveniences. An unknown level coerces to "info" so a typo can't trap
// the instance (the WIT enum would otherwise reject it).
(globalThis as Record<string, unknown>).log = {
  emit: (level: string, message: unknown): void => {
    const lvl: LogLevel = (LOG_LEVELS as readonly string[]).includes(level)
      ? (level as LogLevel)
      : "info";
    logEmit(lvl, stringifyArg(message));
  },
  debug: (...args: unknown[]): void => emitAt("debug", args),
  info: (...args: unknown[]): void => emitAt("info", args),
  warn: (...args: unknown[]): void => emitAt("warn", args),
  error: (...args: unknown[]): void => emitAt("error", args),
};

// Agents reach for `console.log` reflexively regardless of the docs, so
// route the common console methods to the same handler-log channel. Augment
// the existing console object rather than replace it, so other methods
// (`console.dir`, etc.) keep working.
{
  const c = ((globalThis as Record<string, unknown>).console ??
    {}) as Record<string, unknown>;
  c.log = (...args: unknown[]): void => emitAt("info", args);
  c.info = (...args: unknown[]): void => emitAt("info", args);
  c.debug = (...args: unknown[]): void => emitAt("debug", args);
  c.warn = (...args: unknown[]): void => emitAt("warn", args);
  c.error = (...args: unknown[]): void => emitAt("error", args);
  c.trace = (...args: unknown[]): void => emitAt("debug", args);
  (globalThis as Record<string, unknown>).console = c;
}

// StarlingMonkey exposes web-platform network globals (fetch, WebSocket, …)
// that have no host wiring in this engine, so calling them hard-traps the
// wasm instance with an opaque backtrace instead of raising a catchable
// error. WireMirage handlers have no network egress by design (the sandbox
// imports are store / log / clock / response-stream only). Replace the
// egress globals with stubs that throw a clear, catchable Error. Best-effort:
// if a global is locked, leave it rather than break engine init.
function networkUnavailable(name: string): never {
  throw new Error(
    `${name} is not available in WireMirage handlers — handlers have no ` +
      `network access. Mock the upstream as another route instead.`,
  );
}
for (const name of ["fetch", "WebSocket", "EventSource", "XMLHttpRequest"]) {
  try {
    Object.defineProperty(globalThis, name, {
      value: function (): never {
        networkUnavailable(name);
      },
      writable: true,
      configurable: true,
    });
  } catch {
    // Global is non-configurable in this engine build; leave it as-is.
  }
}

// Convenience accessors over the request's tuple-array fields, so a handler
// can write `req.header("content-type")` instead of
// `req.headers.find(([k]) => k === "content-type")?.[1]`. The arrays
// (`headers` / `pathParams` / `query`) stay as-is; these are additive. Header
// lookup is case-insensitive (HTTP semantics); path-params and query keys
// match exactly. Each returns the first match's value, or `undefined`.
function tupleLookup(
  pairs: unknown,
  name: string,
  caseInsensitive: boolean,
): string | undefined {
  if (!Array.isArray(pairs)) return undefined;
  const target = caseInsensitive ? name.toLowerCase() : name;
  for (const pair of pairs) {
    if (Array.isArray(pair) && pair.length >= 2) {
      const key = caseInsensitive ? String(pair[0]).toLowerCase() : pair[0];
      if (key === target) return pair[1] as string;
    }
  }
  return undefined;
}

function withAccessors(req: unknown): unknown {
  if (req === null || typeof req !== "object") return req;
  const r = req as Record<string, unknown>;
  try {
    r.header = (name: string): string | undefined =>
      tupleLookup(r.headers, name, true);
    r.pathParam = (name: string): string | undefined =>
      tupleLookup(r.pathParams, name, false);
    r.queryParam = (name: string): string | undefined =>
      tupleLookup(r.query, name, false);
  } catch {
    // Request object is non-extensible in this build — the array fields are
    // still there, so handlers fall back to the `.find(...)` form.
  }
  return r;
}

// ---------------------------------------------------------------------------
// UPSTREAM WORKAROUND — ComponentizeJS#343 (tracked by wiremirage#50)
//
// Lowering a *negative* s64 out of JS traps the guest before the host import
// runs: the value never crosses the boundary and the request dies with an
// opaque "engine-level fault". Positive s64 (including > 2^32), s32, f64 and
// u64-with-bit-63-set are all fine, so this is specifically signed-64-bit
// lowering. Reproduced on componentize-js 0.20.0 and 0.22.0 (current latest)
// with a three-line guest and no WireMirage code involved.
//
// Two store methods take s64 parameters, so both are affected:
//
//   * `list-range(key, start, stop)` — the contract says a negative index
//     counts from the end. We keep that promise by resolving negatives to
//     their non-negative equivalents here, using the same rule the host
//     applies (clamp to 0), so the visible semantics are unchanged.
//
//   * `incr(key, by)` — a negative delta has no non-negative equivalent, so
//     it can't be worked around. Throw a legible error instead of letting the
//     handler trap with no explanation.
//
// TO REMOVE (when ComponentizeJS#343 is fixed and the engine is rebuilt on a
// release containing the fix): delete `wrapBucketForNegativeS64` and the two
// call sites below, then drop the negative-index notes from
// `crates/wm-host/src/capabilities.rs` and `docs/handlers.md`. The tier-2
// test `negative_list_range_indices_count_from_the_end` asserts the
// *semantics*, not the workaround, so it must keep passing either way — it is
// the check that says removal was safe.
// ---------------------------------------------------------------------------

/** Mirror of the host's index normalisation, so both agree on edge cases. */
function normalizeIndex(i: bigint, len: bigint): bigint {
  if (i >= 0n) return i;
  const from_end = len + i;
  return from_end > 0n ? from_end : 0n;
}

function wrapBucketForNegativeS64(bucket: any): any {
  if (bucket === null || typeof bucket !== "object") return bucket;
  return new Proxy(bucket, {
    get(target, prop, receiver) {
      if (prop === "listRange") {
        return (key: string, start: bigint | number, stop: bigint | number) => {
          let s = BigInt(start);
          let e = BigInt(stop);
          if (s < 0n || e < 0n) {
            const len = BigInt(target.listLength(key));
            if (len === 0n) return [];
            s = normalizeIndex(s, len);
            e = normalizeIndex(e, len);
          }
          return target.listRange(key, s, e);
        };
      }
      if (prop === "incr") {
        return (key: string, by: bigint | number) => {
          const delta = BigInt(by);
          if (delta < 0n) {
            throw new Error(
              "incr(): negative deltas are not supported on the TypeScript/" +
                "JavaScript handler path — a ComponentizeJS defect " +
                "(bytecodealliance/ComponentizeJS#343) traps the handler when a " +
                "negative s64 crosses the component boundary. Count upwards, or " +
                "keep the value with set() instead of incr().",
            );
          }
          return target.incr(key, delta);
        };
      }
      const value = Reflect.get(target, prop, receiver);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}

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
    const result = (userHandle as (a: unknown, b: unknown, c: unknown) => unknown)(
      withAccessors(req),
      wrapBucketForNegativeS64(routeStore),
      wrapBucketForNegativeS64(groupStore),
    );
    // A streaming handler emits its body via `host.responseStream` and
    // may return nothing. The host ignores this return value once
    // `start` was called, but the WIT export still needs a valid
    // `response` record, so coalesce null/undefined to an empty 200.
    if (result === null || result === undefined) {
      return { status: 200, headers: [], body: new Uint8Array() };
    }
    return result;
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

