#!/usr/bin/env bash
# Seed a freshly-booted `just run-web` host with a small set of
# groups + routes + traffic so the UI has something to render.
# Re-runs are idempotent: groups already created get skipped.
#
# Prereqs: a host reachable at $WM_HOST (default localhost:8080)
# booted with WM_BOOTSTRAP_TOKEN=wmt_dev_local. The bundled
# `just run-web` target uses exactly those values.
#
# Storage is in-memory under `just run-web`, so the seeded data
# disappears when the host is stopped. Re-run this script after
# a restart to get back to the same starting point.

set -euo pipefail

HOST="${WM_HOST:-http://localhost:8080}"
TOKEN="${WM_TOKEN:-wmt_dev_local}"
export WM_HOST WM_TOKEN
WM_HOST="$HOST"
WM_TOKEN="$TOKEN"

# Locate the fixture components built by wm-host/build.rs. Walk the
# possible target dirs (debug + release, plus rev-suffixed `out/`)
# and pick the most recent matches so a fresh `cargo build` gets
# preferred over a stale one.
find_fixture() {
  local name="$1"
  local match
  match="$(find target -type f -name "${name}.component.wasm" -printf '%T@ %p\n' 2>/dev/null \
    | sort -nr | head -1 | cut -d' ' -f2-)"
  if [[ -z "$match" ]]; then
    echo "error: ${name}.component.wasm not found under target/." >&2
    echo "  Run 'cargo build -p wm-host' first to compile the fixtures." >&2
    exit 1
  fi
  echo "$match"
}

ECHO_WASM="$(find_fixture echo_handler)"
COUNTER_WASM="$(find_fixture counter_handler)"

CLI=(cargo run -p wm-cli --quiet --)

say() { printf '\n\033[1;36m== %s\033[0m\n' "$1"; }

# Probe — fail fast with a clear message if the host isn't up.
if ! curl -fsS -o /dev/null "$HOST/__health"; then
  echo "error: can't reach $HOST/__health." >&2
  echo "  Start the host first with: just run-web" >&2
  exit 1
fi

# -- Groups -----------------------------------------------------------------

say "Groups"

create_group() {
  # `wm groups create` returns 409 if the group exists; treat that
  # as already-seeded and move on.
  local name="$1"
  shift
  if "${CLI[@]}" groups create "$name" "$@" >/dev/null 2>&1; then
    echo "  + $name"
  else
    echo "  · $name (already exists)"
  fi
}

create_group stripe-mock        --ttl-seconds 86400
create_group openai-mock-flaky  --ttl-seconds 21600
create_group pubsub-fixture     --ttl-seconds 172800 --no-sliding

# -- Routes -----------------------------------------------------------------
# Pre-built wasm fixtures (`echo` returns the request body; `counter`
# increments a kv counter and returns its value). No sidecar required.

say "Routes"

add_route() {
  local group="$1" method="$2" path="$3" wasm="$4"
  if "${CLI[@]}" routes add \
    --group "$group" --method "$method" --path "$path" \
    --wasm-file "$wasm" --bindings-version 0.1.0 >/dev/null 2>&1; then
    echo "  + $group  $method $path"
  else
    echo "  · $group  $method $path (already exists or skipped)"
  fi
}

add_route stripe-mock         POST /v1/charges            "$ECHO_WASM"
add_route stripe-mock         POST /v1/customers          "$ECHO_WASM"
add_route stripe-mock         GET  /v1/charges/{id}       "$ECHO_WASM"
add_route stripe-mock         POST /v1/refunds            "$ECHO_WASM"
add_route openai-mock-flaky   POST /v1/chat/completions   "$COUNTER_WASM"
add_route openai-mock-flaky   GET  /v1/models             "$ECHO_WASM"
add_route pubsub-fixture      POST /webhooks/stripe       "$ECHO_WASM"

# -- Drive a bit of traffic ------------------------------------------------
# So the Live activity / hits_total columns aren't all zeros. Mock
# traffic doesn't need an Authorization header by design.

say "Traffic (so the Last hit / Hits columns aren't empty)"

fire() {
  local method="$1" path="$2" times="$3"
  for _ in $(seq 1 "$times"); do
    curl -fsS -o /dev/null -X "$method" "$HOST$path" \
      -H 'content-type: application/json' \
      -d '{"seed":true}' || true
  done
  echo "  ${times}x  $method $path"
}

fire POST /v1/charges            12
fire POST /v1/customers           5
fire GET  /v1/charges/ch_abc123   3
fire POST /v1/chat/completions    8
fire POST /webhooks/stripe        2

# -- Summary ---------------------------------------------------------------

say "Summary"
"${CLI[@]}" groups list || true
echo
echo "Done. Visit $HOST/__ui/ and log in as admin / devpassword."
echo "The seeded routes are owned by the bootstrap user, so the admin's"
echo "'Just mine' filter will show nothing until you create routes from"
echo "the UI yourself (those land with admin as owner)."
