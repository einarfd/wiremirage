// WireMirage handler SDK — minimal response helpers.
//
// Handlers don't have to use this SDK; the WIT contract maps directly to
// JS records. The helpers here exist to spare authors the bookkeeping of
// `headers: [["content-type", "..."]]` and `body: new TextEncoder().encode(...)`
// for the common content types.

export interface Response {
  status: number;
  headers: Array<[string, string]>;
  body: Uint8Array;
}

const ENC = new TextEncoder();

/** Build a `text/plain` response. */
export function text(body: string, status = 200): Response {
  return {
    status,
    headers: [["content-type", "text/plain; charset=utf-8"]],
    body: ENC.encode(body),
  };
}

/** Build an `application/json` response by JSON-encoding `value`. */
export function json(value: unknown, status = 200): Response {
  return {
    status,
    headers: [["content-type", "application/json; charset=utf-8"]],
    body: ENC.encode(JSON.stringify(value)),
  };
}

/** Build an empty-bodied response with the given status. */
export function empty(status = 204): Response {
  return { status, headers: [], body: new Uint8Array() };
}

/** Build a response with raw bytes and a caller-provided content-type. */
export function bytes(
  body: Uint8Array,
  contentType: string,
  status = 200,
): Response {
  return { status, headers: [["content-type", contentType]], body };
}
