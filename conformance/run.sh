#!/usr/bin/env bash
# Conformance runner. Boots wm-host (in-memory, native via cargo), then for
# each lane: imports its group spec (spec.json) and runs the lane's
# conformance client in Docker against the host.
#
#   ./run.sh                  # run every lane
#   ./run.sh openai-streaming # run one lane
#
# A "lane" is a subdirectory with a Dockerfile + spec.json (a group spec:
# a group name + routes referencing their handler by source_file). The client
# runs in Docker (--network host) so each lane brings its own language/SDK
# toolchain and the host machine only needs Docker + jq + a buildable wm-host.
# We already depend on Docker (the js-engine build), so this adds no new host
# dependency. Linux-oriented: --network host + the host's loopback bind is how
# the container reaches the host (matches CI + the dev VM).
#
# Mock traffic is served on the group's subdomain `{group}.{apex}` (ADR-0030
# virtual-host routing); the apex (localhost) is control-plane only. Each
# client addresses `http://{group}.localhost:PORT`, with --add-host resolving
# that label to loopback inside the container — no DNS needed, the host derives
# the group from the Host header.
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

# Port 0 by default: the OS assigns a free one and the host logs the resolved
# address (it logs `listener.local_addr()`, not the requested string), so we
# read the real port back out of the log. Race-free, unlike probing for a free
# port and hoping it's still free a moment later — and it means a conformance
# run no longer collides with a `just run-web` already holding 8080, or with
# another conformance run. Set WM_PORT to pin one.
PORT="${WM_PORT:-0}"
TOKEN="wmt_conformance_$$"

# --- build the host, then boot it (native, in-memory) ---
# Build as its own step rather than letting `cargo run` compile inside the
# readiness wait. A cold workspace build (swc + wasmtime + cranelift, plus the
# js-engine Docker stage in build.rs) takes minutes, which on a cache-less CI
# runner overran the wait and reported "host never became ready" while rustc
# was still going. Separating them means the wait covers process startup only,
# and a compile error surfaces as a compile error.
echo "building wm-host ..."
( cd "$ROOT" && cargo build -p wm-host )
HOST_BIN="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps \
  | jq -r '.target_directory')/debug/wm-host"

# Host output goes to a log we can parse for the assigned port, and is
# mirrored to stdout in the background so a failing lane still shows the
# host's side of the story in the CI log.
HOST_LOG="$(mktemp -t wm-conformance-host.XXXXXX.log)"
WM_STORAGE=memory WM_BOOTSTRAP_TOKEN="$TOKEN" \
  WM_BOOTSTRAP_EMAIL=conformance@local WM_LISTEN_ADDR="127.0.0.1:${PORT}" \
  "$HOST_BIN" >"$HOST_LOG" 2>&1 &
HOST_PID=$!
tail -f "$HOST_LOG" 2>/dev/null &
TAIL_PID=$!
trap 'kill "$HOST_PID" "$TAIL_PID" 2>/dev/null || true; rm -f "$HOST_LOG"' EXIT

# Wait for the host to listen. On a cold machine first boot Cranelift-compiles
# the ~12 MB StarlingMonkey engine component before binding (runtime.rs says
# ~30 s in debug; an unoptimised Cranelift on a 2-core CI runner has taken
# over 2 min). The result is memoized to a .cwasm in the temp dir, so a dev
# box that has run the tests binds in ~200 ms and a fresh runner does not —
# a range too wide to pick a wall-clock number for, and guessing one is how
# this loop failed twice.
#
# So gate on liveness, not the clock: keep waiting while the process is
# alive, and exit the moment it isn't (bad config, port in use). The cap is
# a backstop against a genuinely wedged host, not a startup estimate — the
# job timeout is the real outer bound. Progress is printed so a slow first
# boot reads as "compiling", not "hung". Every probe is time-bounded: a port
# that accepts a connection but never answers (something else squatting on it)
# hangs a bare `curl` indefinitely, stranding the loop before it re-checks
# liveness — the cap counts iterations, so it would never fire.
echo "waiting for host to listen ..."
BASE=""
for i in $(seq 1 600); do
  # The host logs the bound address once it is actually listening, so the
  # log line is both the readiness signal and the source of the port.
  # One sed, not a grep pipeline: `grep -o '[0-9]*$'` looks right but the log
  # line ends in a quote, so the pattern only ever matches the empty string —
  # GNU grep skips empty matches and exits 1, and the port never appears.
  # `|| true` because until the line is written the pipeline fails, and under
  # `set -e` a failing command substitution kills the script with no message.
  addr=$(sed -n 's/.*"addr":"127\.0\.0\.1:\([0-9]\{1,\}\)".*/\1/p' "$HOST_LOG" 2>/dev/null | head -1 || true)
  if [ -n "$addr" ]; then BASE="http://localhost:${addr}"; break; fi
  kill -0 "$HOST_PID" 2>/dev/null || { echo "host exited during startup"; wait "$HOST_PID" || true; exit 1; }
  [ $((i % 30)) -eq 0 ] && echo "  ... still starting (${i}s; first boot compiles the JS engine)"
  sleep 1
done
[ -n "$BASE" ] || { echo "host alive but never listened after 600s"; exit 1; }
PORT="${BASE##*:}"

# Listening is not the same as serving, so still confirm /health answers.
# Bounded so a socket that accepts but never replies can't hang the run.
curl -fsS --connect-timeout 2 --max-time 10 --retry 5 --retry-delay 1 \
  "${BASE}/health" >/dev/null || { echo "host listening on ${BASE} but /health failed"; exit 1; }
echo "host ready on ${BASE}"

fail=0
for lane in "${LANES[@]}"; do
  [ -f "${lane}/Dockerfile" ] || { echo "no such lane: ${lane}"; fail=1; continue; }
  echo
  echo "=== conformance lane: ${lane} ==="

  # Each lane is a group spec (spec.json): a group name + routes that reference
  # their handler by source_file. Inline each source_file into `source`
  # (jq -Rs JSON-encodes the file, so handler quoting/newlines survive intact)
  # and import the whole group in one shot via POST /api/groups/import — the
  # same spec round-trip the CLI / MCP / UI use (ADR-0030). The group name
  # doubles as the subdomain the client addresses mock traffic on.
  spec="${lane}/spec.json"
  group=$(jq -r '.name' "$spec")
  routes='[]'
  while read -r route; do
    sf=$(jq -r '.source_file' <<<"$route")
    src=$(jq -Rs '.' < "${lane}/${sf}")   # JSON string literal of the source
    route=$(jq -c --argjson src "$src" 'del(.source_file) | .source = $src' <<<"$route")
    routes=$(jq -c --argjson r "$route" '. + [$r]' <<<"$routes")
  done < <(jq -c '.routes[]' "$spec")
  payload=$(jq -c --argjson r "$routes" '.routes = $r' "$spec")
  curl -fsS -X POST "${BASE}/api/groups/import" \
    -H "authorization: Bearer ${TOKEN}" -H 'content-type: application/json' \
    -d "$payload" >/dev/null
  echo "  imported group ${group} ($(jq '.routes | length' <<<"$payload") route(s))"

  # Optional per-lane seeding (e.g. PUT group state), run from the lane dir
  # with (base, token). State isn't part of the routes-only spec, so a lane
  # that needs seeded state does it here (control-plane, on the apex).
  if [ -f "${lane}/setup.sh" ]; then
    ( cd "$lane" && bash setup.sh "$BASE" "$TOKEN" )
  fi

  # Build + run the lane's client in Docker. WM_BASE points at the group
  # subdomain; --add-host makes `{group}.localhost` resolve to loopback in the
  # container (the host serves mock traffic by Host header, ADR-0030).
  img="wiremirage-conformance-${lane}"
  docker build -q -t "$img" "${lane}" >/dev/null
  docker run --rm --network host \
    --add-host "${group}.localhost:127.0.0.1" \
    -e "WM_BASE=http://${group}.localhost:${PORT}" "$img" || fail=1
done

exit "$fail"
