// WireMirage mock of OpenAI's POST /v1/chat/completions, covering both
// the streaming (SSE) and buffered JSON shapes. Built to be conformant
// enough that the *real* `openai` Python client (pydantic-validated)
// accepts every chunk — see conformance/openai-streaming/.
//
// Behavior is deterministic so the conformance test can assert on it:
// the assistant "reply" is `You said: <last user message>`, streamed one
// whitespace-delimited token per SSE frame.

export function handle(req, _routeStore, _groupStore) {
  const body = parseJsonBody(req);
  const model = typeof body.model === "string" ? body.model : "wiremirage-mock";
  const wantStream = body.stream === true;

  const lastUser = lastUserMessage(body.messages);
  const replyText = "You said: " + lastUser;
  const id = "chatcmpl-" + pseudoId();
  const created = Math.floor(host.wallTimeMs() / 1000);

  if (!wantStream) {
    return jsonResponse(200, {
      id,
      object: "chat.completion",
      created,
      model,
      choices: [
        {
          index: 0,
          message: { role: "assistant", content: replyText },
          finish_reason: "stop",
          logprobs: null,
        },
      ],
      usage: {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
      },
    });
  }

  const out = host.responseStream({
    status: 200,
    headers: [
      ["content-type", "text/event-stream"],
      ["cache-control", "no-cache"],
    ],
  });

  const tokens = replyText.split(/\s+/).filter((t) => t.length > 0);
  // Parameterize inter-token pacing via the request body. The `openai`
  // client forwards unknown keys verbatim through `extra_body=`, so a
  // caller can drive the mock's timing without any out-of-band channel.
  const delayMs = Number(body.mock_delay_ms) || 0;

  // First chunk announces the assistant role with empty content — this
  // is what the real API does and what some clients key off.
  if (!out.write(sse({ id, object: "chat.completion.chunk", created, model,
    choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] }))) return;

  for (let i = 0; i < tokens.length; i++) {
    const content = (i === 0 ? "" : " ") + tokens[i];
    if (!out.write(sse({ id, object: "chat.completion.chunk", created, model,
      choices: [{ index: 0, delta: { content }, finish_reason: null }] }))) return;
    if (delayMs > 0) host.sleep(delayMs);
  }

  out.write(sse({ id, object: "chat.completion.chunk", created, model,
    choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }));
  out.write("data: [DONE]\n\n");
  out.close();
}

function sse(obj) {
  return "data: " + JSON.stringify(obj) + "\n\n";
}

function parseJsonBody(req) {
  try {
    const bytes = req && req.body ? new Uint8Array(req.body) : new Uint8Array();
    if (bytes.length === 0) return {};
    return JSON.parse(new TextDecoder().decode(bytes));
  } catch (_e) {
    return {};
  }
}

function lastUserMessage(messages) {
  if (!Array.isArray(messages)) return "";
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i] && messages[i].role === "user") {
      const c = messages[i].content;
      return typeof c === "string" ? c : JSON.stringify(c);
    }
  }
  return "";
}

function pseudoId() {
  return Math.random().toString(36).slice(2, 12);
}

function jsonResponse(status, obj) {
  return {
    status,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify(obj)),
  };
}
