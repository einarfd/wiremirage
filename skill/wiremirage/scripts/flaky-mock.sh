#!/usr/bin/env bash
#
# flaky-mock.sh — provision a single route at PATH that returns 503
# on every Nth call (default every 3rd). Useful for testing retry,
# timeout, and circuit-breaker behavior in the SUT.
#
# The handler keeps a counter in its per-route store, so the "every
# Nth" pattern is deterministic per process: call 1 succeeds, call 2
# succeeds, call 3 fails, call 4 succeeds, ... Reset the counter
# with `reset-state.sh GROUP`.
#
# Usage:
#   WM_HOST=...  WM_TOKEN=...  ./flaky-mock.sh PATH [EVERY_N]
#
# Examples:
#   ./flaky-mock.sh /v1/payments
#   ./flaky-mock.sh /v1/payments 5    # fail every 5th call instead
#
# Created in the group `flaky-mock`. Tear down with:
#   wm groups delete flaky-mock --force

set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 PATH [EVERY_N]" >&2
  exit 2
fi

ROUTE_PATH="$1"
EVERY_N="${2:-3}"
GROUP="${GROUP:-flaky-mock}"

case "$EVERY_N" in
  ''|*[!0-9]*)
    echo "error: EVERY_N must be a positive integer (got: $EVERY_N)" >&2
    exit 2
    ;;
esac
if [[ "$EVERY_N" -lt 2 ]]; then
  echo "error: EVERY_N must be >= 2 (returning 503 on every call doesn't" \
       "exercise retry logic)" >&2
  exit 2
fi

# Idempotent setup: leave an existing flaky-mock group alone, in case
# the caller is iterating.
if ! wm groups show "$GROUP" >/dev/null 2>&1; then
  wm groups create "$GROUP" >/dev/null
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/handler.ts" <<EOF
// Returns 503 on every ${EVERY_N}th call, 200 otherwise. Counter
// lives in the per-route store, so it persists across requests but
// can be cleared with \`wm groups state $GROUP --clear\`.
//
// Note: \`store.incr\` is a WIT s64 — it returns a JS bigint, and
// \`by\` must also be a bigint literal (1n). The handler converts
// to Number for the JSON payload.
export function handle(_req, routeStore, _group) {
  const n = routeStore.incr("count", 1n);
  const failOnEvery = ${EVERY_N}n;
  if (n % failOnEvery === 0n) {
    return {
      status: 503,
      headers: [["content-type", "application/json"]],
      body: new TextEncoder().encode(JSON.stringify({
        error: "service_unavailable",
        message: "Simulated flaky response",
        call_number: Number(n),
      })),
    };
  }
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(JSON.stringify({
      ok: true,
      call_number: Number(n),
    })),
  };
}
EOF

wm routes add --group "$GROUP" --method ANY --path "$ROUTE_PATH" \
  --source-file "$work/handler.ts" >/dev/null

echo "Created flaky route: ANY $ROUTE_PATH (group: $GROUP, fails every ${EVERY_N}th call)"
echo
echo "Try it:"
for i in 1 2 3 4 5; do
  echo "  curl \$WM_HOST$ROUTE_PATH    # call $i"
done
echo
echo "Reset the counter with:"
echo "  wm groups state $GROUP --clear"
