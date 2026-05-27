#!/usr/bin/env bash
#
# streaming-llm-mock.sh — provision a route at PATH that streams an
# OpenAI-style chat-completion response token-by-token over
# Server-Sent Events, the way a real streaming LLM endpoint does.
# Useful for testing a client/gateway's handling of streamed
# responses: partial reads, inter-token latency, time-to-first-token,
# and what happens when the stream is slow.
#
# The handler uses `host.responseStream` (ADR-0022): it commits the
# SSE head, then writes one `data: {...}` frame per token with a sleep
# between frames so the client sees them arrive incrementally — not
# buffered and flushed at the end. Pacing is tunable:
#
#   DELAY_MS  — milliseconds between tokens (default 60)
#   PROMPT    — the text streamed back, one whitespace-token per frame
#               (default "Hello from a streamed WireMirage mock")
#
# Usage:
#   WM_HOST=...  WM_TOKEN=...  ./streaming-llm-mock.sh PATH
#
# Examples:
#   ./streaming-llm-mock.sh /v1/chat/completions
#   DELAY_MS=150 PROMPT="one two three" \
#     ./streaming-llm-mock.sh /v1/chat/completions   # slow, 3 tokens
#
# Created in the group `streaming-llm-mock`.
#
# Tear down with:
#   wm groups delete streaming-llm-mock --force

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 PATH" >&2
  echo "       (set DELAY_MS, PROMPT to tune the stream)" >&2
  exit 2
fi

ROUTE_PATH="$1"
GROUP="${GROUP:-streaming-llm-mock}"
DELAY_MS="${DELAY_MS:-60}"
PROMPT="${PROMPT:-Hello from a streamed WireMirage mock}"

case "$DELAY_MS" in
  ''|*[!0-9]*)
    echo "error: DELAY_MS must be a non-negative integer (got: $DELAY_MS)" >&2
    exit 2
    ;;
esac

: "${WM_HOST:?set WM_HOST (e.g. https://wm.example.com)}"
: "${WM_TOKEN:?set WM_TOKEN to a valid API token}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The prompt is interpolated into the handler as a JSON string so any
# quotes/backslashes are escaped safely. The handler splits it on
# whitespace and streams one token per SSE frame.
prompt_json="$(printf '%s' "$PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || printf '"%s"' "$PROMPT")"

cat > "$work/handler.ts" <<EOF
// streaming-llm-mock handler — streams an OpenAI-style chat completion
// token-by-token over SSE (ADR-0022 / host.responseStream). Each token
// is its own \`data: {...}\` frame, paced by host.sleep so the client
// observes inter-token latency. Ends with the \`data: [DONE]\` sentinel.

export function handle(_req, _routeStore, _groupStore) {
  const out = host.responseStream({
    status: 200,
    headers: [
      ["content-type", "text/event-stream"],
      ["cache-control", "no-cache"],
    ],
  });

  const id = "chatcmpl-" + Math.random().toString(36).slice(2, 10);
  const tokens = ${prompt_json}.split(/\s+/).filter((t) => t.length > 0);

  for (let i = 0; i < tokens.length; i++) {
    // A space prefix on every token but the first mimics how real
    // tokenizers emit leading-space word-pieces.
    const content = (i === 0 ? "" : " ") + tokens[i];
    const frame = {
      id,
      object: "chat.completion.chunk",
      choices: [{ index: 0, delta: { content }, finish_reason: null }],
    };
    // write() returns false once the client has disconnected — stop
    // early so we don't keep working for a reader that's gone.
    if (!out.write("data: " + JSON.stringify(frame) + "\n\n")) return;
    host.sleep(${DELAY_MS});
  }

  const done = {
    id,
    object: "chat.completion.chunk",
    choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
  };
  out.write("data: " + JSON.stringify(done) + "\n\n");
  out.write("data: [DONE]\n\n");
  out.close();
}
EOF

# Group is idempotent — re-running reuses the existing group.
if ! wm groups show "$GROUP" >/dev/null 2>&1; then
  wm groups create "$GROUP" --ttl 1h >/dev/null
fi

wm routes add --group "$GROUP" --method POST --path "$ROUTE_PATH" \
  --source-file "$work/handler.ts" >/dev/null

echo "Created streaming route: POST $ROUTE_PATH (group: $GROUP)"
echo "  delay = ${DELAY_MS}ms/token, prompt = ${PROMPT}"
echo
echo "Watch it stream (curl -N disables buffering so frames print as they arrive):"
echo "  curl -N -X POST \$WM_HOST$ROUTE_PATH"
echo
echo "Inspect without a client (dry-run collects the streamed frames):"
echo "  wm routes test $GROUP/1 --method POST"
