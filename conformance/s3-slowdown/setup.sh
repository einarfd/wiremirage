#!/usr/bin/env bash
# Per-lane seeding, run by ../run.sh after the routes are registered.
# Args: $1 = base URL, $2 = bearer token (unused here — mock traffic is open).
# POSTs the injection rules to the config route, which stores them in group
# state for inject.ts to read.
set -euo pipefail
BASE="$1"
curl -fsS -X POST "${BASE}/_inject_rules" \
  -H 'content-type: application/json' --data-binary @rules.json >/dev/null
echo "  seeded injection rules ($(wc -c < rules.json | tr -d ' ') bytes)"
