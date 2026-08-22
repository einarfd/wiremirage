// Ambient declarations for the engine world's host imports.
//
// componentize-js provides these at runtime; nothing resolves them at
// build time, so they are declared here rather than in engine.ts.
// The distinction matters: `declare module` inside a file that has its
// own imports is a module *augmentation*, and TypeScript refuses to
// augment a module it cannot find ("TS2664: Invalid module name in
// augmentation"). In a .d.ts with no top-level import or export, the
// same syntax declares an ambient module instead — which is what these
// are (ADR-0038).
//
// Keep in step with `wit/engine.wit` and `wit/wiremirage.wit`. A drifted
// declaration here is worse than none: it type-checks a contract the
// host does not implement.

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

declare module "wiremirage:handler/response-stream@0.1.0" {
  /** Commit status + headers; switch the response to streaming mode (ADR-0022). */
  export function start(status: number, headers: [string, string][]): void;
  /** Flush a body chunk. Returns false once the client has disconnected. */
  export function writeChunk(bytes: Uint8Array): boolean;
  /** End the streamed body. */
  export function finish(): void;
}

declare module "wiremirage:handler/callback@0.1.0" {
  /**
   * Schedule an outbound callback (ADR-0034). Throws synchronously when
   * callbacks aren't available (host egress off, or the group hasn't opted
   * in). The host fires it once, after `delayMs`, after the response is sent.
   */
  export function schedule(
    url: string,
    method: string,
    headers: [string, string][],
    body: Uint8Array,
    delayMs: bigint,
  ): void;
}

declare module "wiremirage:handler/log@0.1.0" {
  /** Emit a handler log line; it attaches to this request's journal entry. */
  export function emit(
    level: "debug" | "info" | "warn" | "error",
    message: string,
  ): void;
}
