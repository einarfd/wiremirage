#!/usr/bin/env bash
# Boot wm-host (in-memory), register the OpenAI mock + error routes, and run
# the real `openai` client against them. Opt-in / manual — not part of
# `just check`. Requires: a buildable host (Docker for the js-engine, per the
# repo's build deps) and python3.
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(git rev-parse --show-toplevel)"

PORT="${WM_PORT:-8080}"
BASE="http://localhost:${PORT}"
TOKEN="wmt_conformance_$$"

# --- python client ---
if [ ! -d .venv ]; then python3 -m venv .venv; fi
./.venv/bin/pip install -q -r requirements.txt

# --- boot the host ---
( cd "$ROOT" && WM_STORAGE=memory WM_BOOTSTRAP_TOKEN="$TOKEN" \
    cargo run -q -p wm-host ) &
HOST_PID=$!
cleanup() { kill "$HOST_PID" 2>/dev/null || true; }
trap cleanup EXIT

echo "waiting for host on ${BASE} ..."
for _ in $(seq 1 180); do
  curl -fsS "${BASE}/__health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS "${BASE}/__health" >/dev/null || { echo "host never became ready"; exit 1; }

# --- register routes ---
register() { # <path> <source-file>
  local body
  body=$(python3 -c "import json,sys; print(json.dumps({'methods':['POST'],'path':sys.argv[1],'language':'typescript','source':open(sys.argv[2]).read()}))" "$1" "$2")
  curl -fsS -X POST "${BASE}/__api/routes" \
    -H "authorization: Bearer ${TOKEN}" -H 'content-type: application/json' \
    -d "$body" >/dev/null
  echo "registered POST $1"
}
register /v1/chat/completions handler.ts
register /v1-error/chat/completions error-handler.ts

# --- run the conformance checks ---
WM_BASE="$BASE" ./.venv/bin/python conformance.py
