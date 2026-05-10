#!/usr/bin/env bash
#
# setup-stripe-mock.sh — provision a `stripe-mock` group with the most
# commonly-mocked Stripe endpoints. Useful as a quick-start for tests
# that exercise Stripe integration code, and as a reference for the
# shape of a multi-route group setup.
#
# Routes created:
#   POST /v1/charges           — create a charge, returns a fake ch_… id
#   GET  /v1/charges/{id}      — return a fixed-shape charge for any id
#   POST /v1/refunds           — create a refund, returns a fake re_… id
#   POST /v1/customers         — create a customer, returns a fake cus_… id
#
# Usage:
#   WM_HOST=...  WM_TOKEN=...  ./setup-stripe-mock.sh
#
# Tear down with:  wm groups delete stripe-mock --force
#
# Requires: bash, wm. Mock traffic is unauthenticated; the SUT does
# not need a token.

set -euo pipefail

GROUP="${GROUP:-stripe-mock}"
TTL_SECONDS="${TTL_SECONDS:-3600}"  # 1 hour by default

# Idempotent: if the group exists already, leave it alone. Re-running
# this script after a group exists would fail at create_route on path
# conflicts otherwise.
if wm groups show "$GROUP" >/dev/null 2>&1; then
  echo "Group '$GROUP' already exists — leaving it as-is." >&2
  exit 0
fi

echo "Creating group '$GROUP' (TTL ${TTL_SECONDS}s, sliding)..."
wm groups create "$GROUP" --ttl-seconds "$TTL_SECONDS" >/dev/null

# Each handler is written into a temp file and shipped via --source-file
# to the host's TS sidecar. We wrap the body in mktemp so the script
# is safe to run with an existing $TMPDIR layout.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/charges-create.ts" <<'EOF'
// POST /v1/charges — return a synthesized charge with whatever amount
// the SUT sent (defaulting to 1000 cents if missing or unparseable).
export function handle(req, _route, _group) {
  let amount = 1000;
  try {
    const parsed = JSON.parse(new TextDecoder().decode(req.body));
    if (typeof parsed.amount === "number") amount = parsed.amount;
  } catch (_) { /* leave default */ }

  const id = "ch_" + Math.random().toString(36).slice(2, 12);
  const body = JSON.stringify({
    id,
    object: "charge",
    amount,
    currency: "usd",
    status: "succeeded",
    paid: true,
  });
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(body),
  };
}
EOF

cat > "$work/charges-get.ts" <<'EOF'
// GET /v1/charges/{id} — return a fixed-shape charge keyed off the
// path parameter. Useful for SUTs that fetch-after-create.
//
// Path params come through as `pathParams: [name, value][]` —
// kebab-case WIT names map to camelCase in the JS binding.
export function handle(req, _route, _group) {
  const params = req.pathParams ?? [];
  const found = params.find((p) => p[0] === "id");
  const id = found ? found[1] : "ch_unknown";
  const body = JSON.stringify({
    id,
    object: "charge",
    amount: 1000,
    currency: "usd",
    status: "succeeded",
    paid: true,
  });
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(body),
  };
}
EOF

cat > "$work/refunds.ts" <<'EOF'
// POST /v1/refunds — return a synthesized refund. SUTs typically just
// look at the id and status.
export function handle(_req, _route, _group) {
  const id = "re_" + Math.random().toString(36).slice(2, 12);
  const body = JSON.stringify({
    id,
    object: "refund",
    status: "succeeded",
  });
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(body),
  };
}
EOF

cat > "$work/customers.ts" <<'EOF'
// POST /v1/customers — return a synthesized customer.
export function handle(_req, _route, _group) {
  const id = "cus_" + Math.random().toString(36).slice(2, 12);
  const body = JSON.stringify({
    id,
    object: "customer",
    email: "test@example.com",
  });
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode(body),
  };
}
EOF

add() {
  local method="$1" path="$2" file="$3"
  echo "  + $method $path"
  wm routes add --group "$GROUP" --method "$method" --path "$path" \
    --source-file "$file" >/dev/null
}

echo "Adding routes:"
add POST "/v1/charges"        "$work/charges-create.ts"
add GET  "/v1/charges/{id}"   "$work/charges-get.ts"
add POST "/v1/refunds"        "$work/refunds.ts"
add POST "/v1/customers"      "$work/customers.ts"

echo
echo "Done. Try it:"
echo "  curl -X POST \$WM_HOST/v1/charges -d '{\"amount\":2500}'"
echo
echo "Inspect with:"
echo "  wm journal list $GROUP"
echo
echo "Clean up with:"
echo "  wm groups delete $GROUP --force"
