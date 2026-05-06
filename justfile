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

run-cli *ARGS:
    cargo run -p wm-cli -- {{ARGS}}

run-mcp:
    cargo run -p wm-mcp
