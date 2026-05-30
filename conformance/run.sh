#!/usr/bin/env bash
# Conformance runner. Boots wm-host (in-memory, native via cargo), then for
# each lane: registers its routes (from routes.json) and runs the lane's
# conformance client in Docker against the host.
#
#   ./run.sh                  # run every lane
#   ./run.sh openai-streaming # run one lane
#
# A "lane" is a subdirectory with a Dockerfile + routes.json. The client runs
# in Docker (--network host) so each lane brings its own language/SDK toolchain
# and the host machine only needs Docker + jq + a buildable wm-host. We already
# depend on Docker (the js-engine build), so this adds no new host dependency.
# Linux-oriented: --network host + the host's loopback bind is how the
# container reaches the host (matches CI + the dev VM).
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(git rev-parse --show-toplevel)"

# Lanes: explicit arg, or every subdir with a Dockerfile.
LANES=()
if [ "$#" -gt 0 ]; then
  LANES=("$@")
else
  for d in */; do [ -f "${d}Dockerfile" ] && LANES+=("${d%/}"); done
fi
[ "${#LANES[@]}" -gt 0 ] || { echo "no conformance lanes found"; exit 1; }

PORT="${WM_PORT:-8080}"
BASE="http://localhost:${PORT}"
TOKEN="wmt_conformance_$$"

# --- boot the host (native, in-memory) ---
( cd "$ROOT" && WM_STORAGE=memory WM_BOOTSTRAP_TOKEN="$TOKEN" WM_LISTEN_ADDR="127.0.0.1:${PORT}" \
    cargo run -q -p wm-host ) &
HOST_PID=$!
trap 'kill "$HOST_PID" 2>/dev/null || true' EXIT

echo "waiting for host on ${BASE} ..."
for _ in $(seq 1 180); do
  curl -fsS "${BASE}/__health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS "${BASE}/__health" >/dev/null || { echo "host never became ready"; exit 1; }

fail=0
for lane in "${LANES[@]}"; do
  [ -f "${lane}/Dockerfile" ] || { echo "no such lane: ${lane}"; fail=1; continue; }
  echo
  echo "=== conformance lane: ${lane} ==="

  # Register the lane's routes. jq -Rs reads the source file as a raw string
  # and JSON-encodes it, so handler quoting/newlines survive intact. An
  # optional per-route "group" name attaches the route to a (pre-created)
  # group so sibling routes share group state.
  while read -r route; do
    path=$(jq -r '.path' <<<"$route")
    src=$(jq -r '.source' <<<"$route")
    methods=$(jq -c '.methods // ["POST"]' <<<"$route")
    group=$(jq -r '.group // empty' <<<"$route")
    if [ -n "$group" ]; then
      # Idempotent: a 409 (already exists) is fine.
      curl -fsS -X POST "${BASE}/__api/groups" \
        -H "authorization: Bearer ${TOKEN}" -H 'content-type: application/json' \
        -d "{\"name\":\"${group}\"}" >/dev/null 2>&1 || true
    fi
    body=$(jq -Rs --argjson m "$methods" --arg p "$path" --arg g "$group" \
      'if $g == "" then {methods:$m, path:$p, language:"typescript", source:.}
       else {group:$g, methods:$m, path:$p, language:"typescript", source:.} end' \
      < "${lane}/${src}")
    curl -fsS -X POST "${BASE}/__api/routes" \
      -H "authorization: Bearer ${TOKEN}" -H 'content-type: application/json' \
      -d "$body" >/dev/null
    echo "  registered ${methods} ${path}${group:+ (group: ${group})}"
  done < <(jq -c '.[]' "${lane}/routes.json")

  # Optional per-lane seeding (e.g. POST config into a mock route), run from
  # the lane dir with (base, token).
  if [ -f "${lane}/setup.sh" ]; then
    ( cd "$lane" && bash setup.sh "$BASE" "$TOKEN" )
  fi

  # Build + run the lane's client in Docker.
  img="wiremirage-conformance-${lane}"
  docker build -q -t "$img" "${lane}" >/dev/null
  docker run --rm --network host -e "WM_BASE=${BASE}" "$img" || fail=1
done

exit "$fail"
