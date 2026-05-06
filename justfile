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
test-valkey:
    cargo test -p wm-host --features valkey-tests --test valkey_storage

build:
    cargo build --workspace

run-host:
    cargo run -p wm-host

run-cli *ARGS:
    cargo run -p wm-cli -- {{ARGS}}

run-mcp:
    cargo run -p wm-mcp
