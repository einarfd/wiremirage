default:
    @just --list

# Run the full check suite (fmt check, clippy, test). Excludes tier-3
# Valkey/Docker tests; use `just check-all` for those.
check: fmt-check clippy test

# Like `check` but also runs the tier-3 testcontainers suite. Requires Docker.
check-all: check test-valkey

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# Tier-3: real Valkey container via testcontainers-rs. Needs Docker.
# Reaps stragglers on exit — testcontainers-rs 0.27 has no ryuk
# integration, and the shared `OnceLock<SharedValkey>` in
# `valkey_storage.rs` leaks at process exit because Rust doesn't run
# Drop on statics. The label filter only matches containers
# testcontainers itself spawned, so the dev compose stack
# (`docker compose up`) is left alone.
test-valkey:
    #!/usr/bin/env bash
    set -e
    cleanup() {
      docker ps -aq --filter 'label=org.testcontainers.managed-by=testcontainers' 2>/dev/null \
        | xargs -r docker rm -f >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    cargo test -p wm-host --features valkey-tests --test valkey_storage

# Conformance lanes: run a real third-party client library (in Docker)
# against a WireMirage mock to smoke out fidelity gaps. Opt-in — NOT part of
# `just check`. No arg runs every lane; pass a name to run one, e.g.
# `just conformance openai-streaming`. Needs Docker + jq + a buildable host.
conformance LANE="":
    ./conformance/run.sh {{ LANE }}

build:
    cargo build --workspace

run-host:
    cargo run -p wm-host

# Run wm-host with the full realistic stack: Valkey (Docker) for
# persistence + the host locally via cargo with local auth + browser
# sessions. After it starts, open http://localhost:8080/__ui/ and log
# in as `admin` / `devpassword`. Data persists across restarts in the
# Valkey volume — `docker compose down -v` to wipe. The `WM_LOCAL_AUTH`
# setting is for local dev only — never use these credentials on a
# publicly-reachable host (see ADR-0018). Source-language handlers
# (JS / TS) compile in-host via the embedded js-engine + swc — no
# external compiler dependency (ADR-0020).
run-web:
    #!/usr/bin/env bash
    set -e
    docker compose up -d valkey
    echo "Waiting for Valkey to be reachable ..."
    until docker compose exec -T valkey valkey-cli ping >/dev/null 2>&1; do
      sleep 0.2
    done
    echo "Valkey ready. Starting host ..."
    echo "  Control plane (UI/API/MCP): http://localhost:8080/__ui/  (log in admin / devpassword)"
    echo "  Mock traffic is per-group (ADR-0030): http://{group}.localhost:8080/...  "
    echo "    e.g. curl -H 'Host: my-group.localhost' http://localhost:8080/v1/charges"
    WM_STORAGE=redis://localhost:6379 \
      WM_BOOTSTRAP_TOKEN=wmt_dev_local \
      WM_LOCAL_AUTH='admin:devpassword:admin,user:devpassword' \
      SESSION_SECRET='dev-only-session-secret-do-not-use-in-prod-32b' \
      cargo run -p wm-host

# Same as `run-web` but with in-memory storage. Faster to start; data
# wipes on host stop. Use this when iterating on host code where
# persistence doesn't matter. UI dogfooding generally wants `run-web`.
run-web-fast:
    WM_STORAGE=memory \
      WM_BOOTSTRAP_TOKEN=wmt_dev_local \
      WM_LOCAL_AUTH='admin:devpassword:admin,user:devpassword' \
      SESSION_SECRET='dev-only-session-secret-do-not-use-in-prod-32b' \
      cargo run -p wm-host

# Run the full stack inside Docker — Valkey + wm-host both via
# `docker compose --profile full`. Uses the same Dockerfile that will
# build the release image, so this is the "is the prod-shaped build
# still working" check, not the inner dev loop. For iterating on host
# code, use `run-web` — cargo's incremental compile is ~5-15s versus
# the ~30-60s warm Docker rebuild this triggers.
#
# After it starts, open http://localhost:8080/__ui/ and log in as
# `admin` / `devpassword`. Stop with `just stop-web-docker` (keeps
# Valkey volume) or `just wipe-dev` (wipes it).
run-web-docker:
    #!/usr/bin/env bash
    set -e
    # The container reads config from `.env` via compose's `env_file`
    # (not shell passthrough), so write the dev creds there. `.env` is
    # gitignored; these are dev-only values, never for production.
    printf '%s\n' \
      'WM_BOOTSTRAP_TOKEN=wmt_dev_local' \
      'WM_LOCAL_AUTH=admin:devpassword:admin,user:devpassword' \
      'SESSION_SECRET=dev-only-session-secret-do-not-use-in-prod-32b' \
      > .env
    docker compose --profile full up -d --build
    echo "Waiting for wm-host to respond on http://localhost:8080/__health ..."
    until curl -fsS http://localhost:8080/__health >/dev/null 2>&1; do
      sleep 0.5
    done
    echo "Ready. Open http://localhost:8080/__ui/ — log in as admin / devpassword."
    echo "Tail logs:  docker compose logs -f wm-host"
    echo "Stop:       just stop-web-docker  (or  just wipe-dev  to also wipe Valkey)"

# Stop the full Docker stack started by `run-web-docker`. Keeps the
# Valkey volume so a subsequent `run-web-docker` reuses existing
# routes/users/etc.; use `wipe-dev` to also wipe Valkey state.
stop-web-docker:
    docker compose --profile full down

# Pour a handful of groups + routes + traffic into a running
# `just run-web` (or `run-web-fast`) host so the UI has something
# to render. Under `run-web`, data persists in Valkey across host
# restarts — re-running this script is idempotent (already-created
# groups/routes are detected and skipped). Under `run-web-fast`,
# everything wipes on host stop; re-run after every restart.
seed-dev:
    ./scripts/seed-dev.sh

# Stop the dev compose stack and wipe the Valkey volume so the next
# `just run-web` starts from an empty store. Useful when a data-shape
# change makes pre-existing records unreadable, or when you just want
# a clean slate to re-seed against. Doesn't touch the js-engine
# builder image used by build.rs. Includes `--profile full` so if
# `run-web-docker` is running, wm-host comes down with it.
wipe-dev:
    docker compose --profile full down -v

run-cli *ARGS:
    cargo run -p wm-cli -- {{ARGS}}

run-mcp:
    cargo run -p wm-mcp
