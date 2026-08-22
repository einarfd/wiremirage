// WireMirage handler API — types for the code you write.
//
// A handler is a single function the host calls per matched request. Drop
// this file next to your handler (or point `tsconfig.json` at it) and your
// editor knows the shape of everything below; run `tsc --noEmit` yourself
// if you want the check. The host does not type-check handlers — it
// transpiles them and runs them (ADR-0038); `wm routes test` is the
// server-side feedback loop.
//
//   export function handle(req, routeStore, groupStore) {
//     return { status: 200, headers: [], body: new TextEncoder().encode("hi") };
//   }
//
// Mirrors `wit/wiremirage.wit`, translated into the shape the JavaScript
// binding actually presents: kebab-case WIT names arrive camelCased, and
// `s64` arrives as `bigint`. Those two facts are the most common source of
// handler bugs, which is most of why this file exists.

/** One request, as the host hands it to a handler. */
export interface WireMirageRequest {
  /** Always uppercase: "GET", "POST", … */
  method: string;
  /** The literal path, e.g. "/users/123/posts/456". */
  path: string;
  /** The route pattern that matched, e.g. "/users/{id}/posts/{post-id}". */
  matchedPattern: string;
  /** Captures from the pattern, in order: `[["id", "123"]]`. */
  pathParams: Array<[string, string]>;
  /** Parsed query parameters; names lowercased. */
  query: Array<[string, string]>;
  /** Request headers; names lowercased. */
  headers: Array<[string, string]>;
  /** Raw body bytes. May be empty. Capped at 10 MiB by the dispatcher. */
  body: Uint8Array;
  /** Case-insensitive single header lookup. */
  header(name: string): string | undefined;
  /** Single path-param lookup by name. */
  pathParam(name: string): string | undefined;
  /** Single query-param lookup by name (first match). */
  queryParam(name: string): string | undefined;
}

/** What a buffered handler returns. A streaming handler returns nothing. */
export interface WireMirageResponse {
  /** Any status code, including non-standard ones. */
  status: number;
  headers: Array<[string, string]>;
  body: Uint8Array;
}

/**
 * A key-value namespace. Handlers get two: one private to the route, one
 * shared by every route in the group. State survives between requests
 * until the group expires or is cleared from outside.
 *
 * Note the `bigint`s: the WIT types are `s64`/`u64`, so `incr` takes and
 * returns `bigint` (`incr("n", 1n)`, not `incr("n", 1)`).
 */
export interface WireMirageStore {
  get(key: string): Uint8Array | null;
  set(key: string, value: Uint8Array): void;
  delete(key: string): void;
  /** Atomic. Returns the new value. Negative deltas are not supported yet — see ComponentizeJS#343. */
  incr(key: string, by: bigint): bigint;
  /** Keys in this store, optionally filtered by prefix; pass null for all. */
  listKeys(prefix: string | null): string[];

  listPush(key: string, value: Uint8Array): void;
  listPop(key: string): Uint8Array | null;
  /** Inclusive range; negative indices count from the end. */
  listRange(key: string, start: bigint, stop: bigint): Uint8Array[];
  listLength(key: string): bigint;

  hashGet(key: string, field: string): Uint8Array | null;
  hashSet(key: string, field: string, value: Uint8Array): void;
  hashDelete(key: string, field: string): void;
  hashKeys(key: string): string[];

  setAdd(key: string, member: string): void;
  setRemove(key: string, member: string): void;
  setContains(key: string, member: string): boolean;
}

/** The writer returned by `host.responseStream` (ADR-0022). */
export interface WireMirageResponseStream {
  /** Flush a chunk. Returns false once the client has disconnected. */
  write(chunk: Uint8Array | ArrayBuffer | string): boolean;
  /** End the streamed body. */
  close(): void;
}

/** The `host` global available inside a handler. */
export interface WireMirageHost {
  /** Block this handler. Counts against the wall-clock budget (~30 s buffered, ~5 min streaming). */
  sleep(ms: number | bigint): void;
  /** Unix epoch milliseconds. May jump on NTP correction. */
  wallTimeMs(): number;
  /** Monotonic milliseconds; meaningful only as a difference. */
  monotonicMs(): number;
  /** Commit status + headers and switch to a streamed body (ADR-0022). */
  responseStream(init?: {
    status?: number;
    headers?: Array<[string, string]>;
  }): WireMirageResponseStream;
  /**
   * Schedule one outbound webhook, fired after this response is sent
   * (ADR-0034). Throws synchronously when egress is off host-wide or the
   * group hasn't opted in — catch it if that's expected.
   */
  scheduleCallback(init: {
    url: string;
    method?: string;
    headers?: Array<[string, string]>;
    body?: Uint8Array | string;
    delayMs?: number;
  }): void;
}

declare global {
  const host: WireMirageHost;
  /**
   * Bodies are bytes, so nearly every handler goes through these. They are
   * runtime globals the engine provides, not ES builtins — which is why
   * they are declared here rather than coming from a `lib`.
   */
  class TextEncoder {
    encode(input?: string): Uint8Array;
  }
  class TextDecoder {
    constructor(label?: string);
    decode(input?: Uint8Array | ArrayBuffer): string;
  }
  /** StarlingMonkey's console goes to the host's stderr, not your journal — use `log`. */
  const console: {
    log(...args: unknown[]): void;
    info(...args: unknown[]): void;
    warn(...args: unknown[]): void;
    error(...args: unknown[]): void;
    debug(...args: unknown[]): void;
  };
  /** Emit a line into this request's journal entry (`wm journal show`). */
  const log: {
    debug(message: string): void;
    info(message: string): void;
    warn(message: string): void;
    error(message: string): void;
  };
}

/**
 * The function every handler declares. Return a response, or return
 * nothing after committing a `host.responseStream`.
 */
export type WireMirageHandler = (
  req: WireMirageRequest,
  routeStore: WireMirageStore,
  groupStore: WireMirageStore,
) => WireMirageResponse | void;
