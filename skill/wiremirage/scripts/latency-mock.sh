#!/usr/bin/env bash
#
# latency-mock.sh — provision a single route at PATH that responds
# with growing latency. The handler tracks "elapsed since first
# request" via the route-private store and sleeps for a duration
# that scales with elapsed time. Useful for reproducing API-gateway
# cascading-failure modes that depend on response time creeping up
# toward a timeout threshold.
#
# The default latency curve adds 50ms per second of elapsed time,
# capped at 30s (the wasm sandbox's per-request wall-clock limit).
# Override with BASE_MS, RAMP_MS_PER_SEC, and CAP_MS:
#
#   delay_ms = min(CAP_MS, BASE_MS + RAMP_MS_PER_SEC * elapsed_seconds)
#
# Usage:
#   WM_HOST=...  WM_TOKEN=...  ./latency-mock.sh PATH
#
# Examples:
#   ./latency-mock.sh /v1/completions
#   BASE_MS=200 RAMP_MS_PER_SEC=100 CAP_MS=10000 \
#     ./latency-mock.sh /v1/completions    # starts at 200ms, ramps 100ms/s
#
# Created in the group `latency-mock`. Reset the start-time clock with
#   wm groups state latency-mock --clear
# (clears the per-route store; the next call records "now" as t=0).
#
# Tear down with:
#   wm groups delete latency-mock --force

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 PATH" >&2
  echo "       (set BASE_MS, RAMP_MS_PER_SEC, CAP_MS to tune the curve)" >&2
  exit 2
fi

ROUTE_PATH="$1"
GROUP="${GROUP:-latency-mock}"
BASE_MS="${BASE_MS:-50}"
RAMP_MS_PER_SEC="${RAMP_MS_PER_SEC:-50}"
CAP_MS="${CAP_MS:-30000}"

for v in BASE_MS RAMP_MS_PER_SEC CAP_MS; do
  case "${!v}" in
    ''|*[!0-9]*)
      echo "error: $v must be a non-negative integer (got: ${!v})" >&2
      exit 2
      ;;
  esac
done

: "${WM_HOST:?set WM_HOST (e.g. https://wm.example.com)}"
: "${WM_TOKEN:?set WM_TOKEN to a valid API token}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/handler.ts" <<EOF
// latency-mock handler — sleeps for a duration that grows with time
// since first call, then returns 200. The growth rate and cap are
// baked in by the deploy script (ADR-0021 / host.sleep).
//
// monotonicMs() is the right clock here — it's process-anchored and
// can't jump backward across NTP corrections, so the elapsed-time
// computation stays correct even if the host clock skews.

export function handle(_req, routeStore, _group) {
  const now = host.monotonicMs();
  const startStr = routeStore.get("first_seen_ms");
  let start;
  if (startStr === null) {
    start = now;
    routeStore.set("first_seen_ms", new TextEncoder().encode(String(now)));
  } else {
    start = Number(new TextDecoder().decode(startStr));
  }
  const elapsed_ms = now - start;
  const elapsed_s = elapsed_ms / 1000;

  const base = ${BASE_MS};
  const ramp = ${RAMP_MS_PER_SEC};
  const cap  = ${CAP_MS};
  const delay = Math.min(cap, Math.trunc(base + ramp * elapsed_s));

  host.sleep(delay);

  const body = new TextEncoder().encode(JSON.stringify({
    ok: true,
    elapsed_ms: elapsed_ms,
    delay_ms: delay,
  }));
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body,
  };
}
EOF

# Group is idempotent — re-running the script reuses the existing
# group instead of erroring on a name collision.
if ! wm groups show "$GROUP" >/dev/null 2>&1; then
  wm groups create "$GROUP" --ttl 1h >/dev/null
fi

wm routes add --group "$GROUP" --method ANY --path "$ROUTE_PATH" \
  --source-file "$work/handler.ts" >/dev/null

echo "Created latency route: ANY $ROUTE_PATH (group: $GROUP)"
echo "  base = ${BASE_MS}ms, ramp = ${RAMP_MS_PER_SEC}ms/s, cap = ${CAP_MS}ms"
echo
echo "Try it (notice the response time grow):"
for i in 1 2 3 4 5; do
  echo "  time curl -s \$WM_HOST$ROUTE_PATH    # call $i"
done
echo
echo "Reset the elapsed-time clock with:"
echo "  wm groups state $GROUP --clear"
