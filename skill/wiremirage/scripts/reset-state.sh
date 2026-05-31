#!/usr/bin/env bash
#
# reset-state.sh — clear all per-route and per-group key-value state
# for a group, without deleting the routes themselves. Call between
# test phases when you want a clean slate but the same routes.
#
# For a reset to a *known baseline* (rather than empty), snapshot once
# (`wm groups state GROUP --snapshot > base.json`) and restore later
# with `wm groups state GROUP --reset-from base.json`.
#
# Usage:
#   WM_HOST=...  WM_TOKEN=...  ./reset-state.sh GROUP
#
# What it touches:
#   - the per-route store of every route in GROUP
#   - the group-shared store
#
# What it leaves:
#   - the routes themselves
#   - the journal (use `wm groups journal --clear GROUP` for that)
#   - the group's TTL / metadata

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 GROUP" >&2
  exit 2
fi

GROUP="$1"

# Confirm the group exists so we surface a clear error rather than
# silently no-op when the caller mistypes.
if ! wm groups show "$GROUP" >/dev/null 2>&1; then
  echo "error: group '$GROUP' not found" >&2
  exit 5
fi

wm groups state "$GROUP" --clear
echo "cleared kv state for group: $GROUP"
