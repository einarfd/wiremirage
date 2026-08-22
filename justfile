default:
    @just --list

# Run the full check suite (fmt check, clippy, test). Excludes tier-3
# Valkey/Docker tests; use `just check-all` for those.
check: fmt-check clippy test

# Like `check` but also runs the tier-3 testcontainers suite. Requires Docker.
check-all: check test-valkey typecheck

# Type-check the shipped handler types (ADR-0038) by compiling
# types/example-handler.ts against types/wiremirage-handler.d.ts. The .d.ts
# is hand-written and describes a contract users write code against, so
# "does a real handler still compile against it" is the thing worth
# asserting; tests/handler_types_track_wit.rs covers the other half, that it
# still matches the WIT.
#
# Runs in the js-engine builder image, which already carries the typescript
# the engine's own `npm run typecheck` uses — no second toolchain to pin.
# That means Docker, which is why this is in check-all and not check.
typecheck:
    #!/usr/bin/env bash
    set -euo pipefail
    docker build --quiet -t wm-js-engine-builder:dev compiler/js-engine >/dev/null
    docker run --rm -v "$PWD/types":/types -w /app wm-js-engine-builder:dev \
      npx tsc --noEmit -p /types/tsconfig.json
    echo "handler types: example-handler.ts compiles against wiremirage-handler.d.ts"

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Policy lives in `deny.toml`; every ignore there has to say why and what
# clears it. Requires cargo-deny (`cargo install cargo-deny --locked`).
#
# Deliberately NOT part of `just check` — it fetches the RustSec advisory
# database, so it needs network and would make the inner loop fail
# offline. CI runs it as its own job on every push and PR.
#
# Dependency audit: advisories, licenses, banned crates, source registries.
audit:
    cargo deny --workspace check

# Run tests with nextest. Parallel test binaries give ~27% speedup over
# `cargo test` on this workspace (2:50 → 2:04 measured). Requires
# `cargo-nextest` installed (`cargo install cargo-nextest --locked`).
# Valkey/Docker tier-3 tests are excluded — those use a shared container
# via OnceLock that breaks under nextest's process-per-test model; use
# `just test-valkey` (still on `cargo test`) for those.
test:
    cargo nextest run --workspace

# Tier-3: real Valkey container via testcontainers-rs. Needs Docker.
# Uses `cargo test` (not nextest) because the shared `OnceLock<SharedValkey>`
# pattern boots one container for the whole test binary — nextest's
# process-per-test model would spawn one container per test (~35x startup
# cost). Reaps stragglers on exit — testcontainers-rs 0.27 has no ryuk
# integration, and the `OnceLock` leaks at process exit because Rust
# doesn't run Drop on statics. The label filter only matches containers
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
# sessions. After it starts, open http://localhost:8080/ui/ and log
# in as `admin@local` / `devpassword`. Data persists across restarts in the
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
    echo "  Control plane (UI/API/MCP): http://localhost:8080/ui/  (log in admin@local / devpassword)"
    echo "  Mock traffic is per-group (ADR-0030): http://{group}.localhost:8080/...  "
    echo "    e.g. curl -H 'Host: my-group.localhost' http://localhost:8080/v1/charges"
    WM_STORAGE=redis://localhost:6379 \
      WM_BOOTSTRAP_TOKEN=wmt_dev_local \
      WM_BOOTSTRAP_EMAIL=admin@local \
      WM_LOCAL_AUTH='admin@local:devpassword:admin,user@local:devpassword' \
      SESSION_SECRET='dev-only-session-secret-do-not-use-in-prod-32b' \
      cargo run -p wm-host

# Same as `run-web` but with in-memory storage. Faster to start; data
# wipes on host stop. Use this when iterating on host code where
# persistence doesn't matter. UI dogfooding generally wants `run-web`.
run-web-fast:
    WM_STORAGE=memory \
      WM_BOOTSTRAP_TOKEN=wmt_dev_local \
      WM_BOOTSTRAP_EMAIL=admin@local \
      WM_LOCAL_AUTH='admin@local:devpassword:admin,user@local:devpassword' \
      SESSION_SECRET='dev-only-session-secret-do-not-use-in-prod-32b' \
      cargo run -p wm-host

# Run the full stack inside Docker — Valkey + wm-host both via compose,
# building the host image from source (the docker-compose.dev.yml override).
# Uses the same Dockerfile CI builds + publishes, so this is the "is the
# prod-shaped build still working" check, not the inner dev loop. For
# iterating on host code, use `run-web` — cargo's incremental compile is
# ~5-15s versus the ~30-60s warm Docker rebuild this triggers. (The plain
# `docker compose --profile full up` — no dev override — pulls the published
# GHCR image instead of building.)
#
# After it starts, open http://localhost:8080/ui/ and log in as
# `admin@local` / `devpassword`. Stop with `just stop-web-docker` (keeps
# Valkey volume) or `just wipe-dev` (wipes it).
run-web-docker:
    #!/usr/bin/env bash
    set -e
    # The container reads config from `.env` via compose's `env_file`
    # (not shell passthrough), so write the dev creds there. `.env` is
    # gitignored; these are dev-only values, never for production.
    printf '%s\n' \
      'WM_BOOTSTRAP_TOKEN=wmt_dev_local' \
      'WM_BOOTSTRAP_EMAIL=admin@local' \
      'WM_LOCAL_AUTH=admin@local:devpassword:admin,user@local:devpassword' \
      'SESSION_SECRET=dev-only-session-secret-do-not-use-in-prod-32b' \
      > .env
    docker compose -f docker-compose.yml -f docker-compose.dev.yml --profile full up -d --build
    echo "Waiting for wm-host to respond on http://localhost:8080/health ..."
    until curl -fsS http://localhost:8080/health >/dev/null 2>&1; do
      sleep 0.5
    done
    echo "Ready. Open http://localhost:8080/ui/ — log in as admin / devpassword."
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

# Re-export the ADRs from the Arkiv design workspace into docs/adr/.
# Maintainer-only: needs the authed `arkiv` CLI built with the publish extra
# (`uv tool install --with markdown2 --with nh3 <arkiv checkout>`). Arkiv is
# where the ADRs are authored; docs/adr/ is a published snapshot, so anything
# wrong in the output gets fixed upstream and re-exported, never patched here.
#
# No `--strict`: the ADRs deliberately reference design docs that stay in the
# workspace (route-model.md, storage-model.md, …), so ~70 "outside the
# published set" warnings are the expected steady state, not a regression.
# Read them for *new* names — a warning about another ADR would mean a real
# broken link.
export-adrs:
    #!/usr/bin/env bash
    set -euo pipefail
    ws=$(arkiv list-workspaces --json | jq -r '.. | select(type == "object" and .slug? == "wiremirage") | .id' | head -1)
    [ -n "$ws" ] || { echo "wiremirage workspace not found (arkiv login?)"; exit 1; }
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
    arkiv publish "$tmp" -w "$ws" --include 'adrs/**' --format md
    rm -f docs/adr/*.md
    cp "$tmp"/adrs/*.md docs/adr/
    echo "re-export complete; review 'git diff docs/adr' before committing"
