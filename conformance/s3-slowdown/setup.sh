#!/usr/bin/env bash
# Per-lane seeding, run by ../run.sh after the routes are registered.
# Args: $1 = base URL, $2 = bearer token.
#
# Seeds the injection rules into the group's shared state via the
# writable-state API (ADR-0025): PUT the rules.json content as the
# value of the gkv key "inject:rules", which inject.ts reads. (Before
# ADR-0025 this needed a dedicated config mock route; the state API
# made that unnecessary — so this lane is also a live check of it.)
set -euo pipefail
BASE="$1"
TOKEN="$2"
body=$(jq -Rs --arg k "inject:rules" '{entries: {($k): .}}' < rules.json)
curl -fsS -X PUT "${BASE}/__api/groups/s3-slowdown/state" \
  -H "authorization: Bearer ${TOKEN}" -H 'content-type: application/json' \
  -d "$body" >/dev/null
echo "  seeded injection rules into group state ($(wc -c < rules.json | tr -d ' ') bytes)"
