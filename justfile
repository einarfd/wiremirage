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
# a clean slate to re-seed against. Doesn't touch the compiler image.
wipe-dev:
    docker compose down -v

run-cli *ARGS:
    cargo run -p wm-cli -- {{ARGS}}

run-mcp:
    cargo run -p wm-mcp
