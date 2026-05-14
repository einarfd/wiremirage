default:
    @just --list

# Run the full check suite (fmt check, clippy, test). Excludes tier-3
# Valkey/Docker tests; use `just check-all` for those.
check: fmt-check clippy test

# Like `check` but also runs the tier-3 testcontainers suite. Requires Docker.
check-all: check test-valkey test-sidecar

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# Tier-3: real Valkey container via testcontainers-rs. Needs Docker.
test-valkey:
    cargo test -p wm-host --features valkey-tests --test valkey_storage

# Tier-3: real TypeScript compiler sidecar via testcontainers-rs. Builds the
# image first, then runs the end-to-end source-based POST tests. Needs Docker.
test-sidecar:
    docker build -f compiler/typescript/Dockerfile -t wiremirage/compiler-typescript:dev .
    cargo test -p wm-host --features sidecar-tests --test sidecar_e2e

# Build the compiler sidecar image without running tests.
build-sidecar-image:
    docker build -f compiler/typescript/Dockerfile -t wiremirage/compiler-typescript:dev .

build:
    cargo build --workspace

run-host:
    cargo run -p wm-host

# Run wm-host with local auth + sessions configured for browser
# dogfood. After it starts, open http://localhost:8080/__ui/ in a
# browser, log in as `admin` / `devpassword`, and click around. The
# `WM_LOCAL_AUTH` setting is for local dev only — never use these
# credentials on a publicly-reachable host (see ADR-0018).
run-web:
    WM_STORAGE=memory \
      WM_BOOTSTRAP_TOKEN=wmt_dev_local \
      WM_LOCAL_AUTH='admin:devpassword:admin,user:devpassword' \
      SESSION_SECRET='dev-only-session-secret-do-not-use-in-prod-32b' \
      cargo run -p wm-host

# Pour a handful of groups + routes + traffic into a running
# `just run-web` host so the UI has something to render. Wipes
# happen at host stop (in-memory storage), so re-run this after
# every restart.
seed-dev:
    ./scripts/seed-dev.sh

run-cli *ARGS:
    cargo run -p wm-cli -- {{ARGS}}

run-mcp:
    cargo run -p wm-mcp
