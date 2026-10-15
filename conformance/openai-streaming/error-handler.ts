// Error-shape mock for OpenAI's POST /v1/chat/completions. The conformance
// client drives which status we emit via the x-mock-status header (the real
// `openai` SDK forwards extra headers verbatim), so one route covers the
// typed-exception mapping for 429 / 400 / 500 — the three shapes clients
// actually branch on. Body shape follows OpenAI's error envelope: a single
// `error` object whose `type` selects the SDK's exception class.
export function handle(req, _r, _g) {
  const status = Number(header(req, "x-mock-status")) || 429;
  const shape = errorShape(status);
  return {
    status,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify(shape)),
  };
}

function errorShape(status) {
  // Same envelope; only message/type/code vary per status, which is what
  // the SDK keys its typed exceptions off.
  const map = {
    400: { message: "Invalid request", type: "invalid_request_error", code: "invalid_request" },
    429: { message: "Rate limit reached", type: "rate_limit_error", code: "rate_limit_exceeded" },
    500: { message: "The server had an error", type: "api_error", code: null },
  };
  const e = map[status] || map[500];
  return { error: Object.assign({ param: null }, e) };
}

function header(req, name) {
  const h = (req.headers || []).find(
    ([k]) => String(k).toLowerCase() === String(name).toLowerCase(),
  );
  return h ? h[1] : null;
}
