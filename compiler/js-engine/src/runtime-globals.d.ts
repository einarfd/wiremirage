// The runtime surface StarlingMonkey hands the engine, declared so the
// type checker knows what exists (ADR-0038).
//
// These are web-platform globals the JS engine component provides — not
// ES language builtins, so `lib: ["es2023"]` doesn't know them. Pulling
// in TypeScript's `dom` lib would type thousands of APIs this runtime
// does not have (and would type `fetch` as *working*, when the engine
// deliberately removes it — see engine.ts's network-globals block).
// Declaring the handful we actually use keeps the checker honest about
// what the sandbox offers.
//
// Anything added here is a claim about StarlingMonkey. If a handler
// traps on a "missing" global that is declared in this file, this file
// is what lied.

declare class TextEncoder {
  encode(input?: string): Uint8Array;
}

declare class TextDecoder {
  constructor(label?: string);
  decode(input?: Uint8Array | ArrayBuffer): string;
}

/** StarlingMonkey's console writes to stderr; the host does not capture it. */
declare const console: {
  log(...args: unknown[]): void;
  info(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
  debug(...args: unknown[]): void;
  [method: string]: unknown;
};
