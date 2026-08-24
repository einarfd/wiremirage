# Multi-stage Dockerfile for the WireMirage host.
#
# Stage 1 (engine-js) transpiles compiler/js-engine/src/engine.ts to
# JavaScript with wm-transpile — the same swc the runtime uses for user
# handlers (ADR-0038). It needs its own stage because the node stage that
# componentizes runs before any Rust would otherwise be built, and the
# alternative (a second, npm-side transpiler) is the version drift that ADR
# rejected. It shares the cargo cache mounts with stage 3, so the swc tree it
# compiles is reused there rather than built twice.
#
# Stage 2 (js-engine-builder) builds the shared js-engine.wasm via
# componentize-js, mirroring compiler/js-engine/Dockerfile. We don't
# reuse that Dockerfile directly because it would require docker-in-
# docker; instead we inline the same steps as a build stage and pass
# the output to the Rust builder via WM_JS_ENGINE_WASM_OVERRIDE so
# build.rs skips its own docker invocation.
#
# Stage 3 (rust-builder) compiles wm-host in release mode.
#
# Stage 4 (runtime) ships the binary on debian:bookworm-slim with
# ca-certificates and libgcc only.

# syntax=docker/dockerfile:1.7


# ── Stage 1: engine.ts → engine.js ───────────────────────────────────────
FROM rust:1-bookworm AS engine-js

WORKDIR /src
COPY . .

# Same cache mount paths as stage 3, so this stage's swc build is not
# repeated there.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p wm-transpile --bin wm-engine-transpile && \
    mkdir -p /out && \
    /src/target/release/wm-engine-transpile \
      compiler/js-engine/src/engine.ts /out/engine.js


# ── Stage 2: js-engine builder ───────────────────────────────────────────
FROM node:24-bookworm-slim@sha256:3638d9a6fe4030bd716be989438248074489337ba3275657f93595428be4fc03 AS js-engine-builder

WORKDIR /app

# npm install layer first so engine source edits don't bust the install cache.
COPY compiler/js-engine/package.json compiler/js-engine/package-lock.json ./
RUN npm ci --silent

COPY compiler/js-engine/src ./src
COPY compiler/js-engine/wit ./wit
COPY compiler/js-engine/build.mjs compiler/js-engine/tsconfig.json ./

# Same checker step compiler/js-engine/Dockerfile runs, for the same reason
# (ADR-0038): typescript is here to check engine.ts, never to emit it. Keeping
# both paths identical is what stops this inlined copy from drifting.
RUN npm run typecheck

# The JS from stage 1. build.mjs consumes it and only componentizes.
COPY --from=engine-js /out/engine.js ./engine.prebuilt.js

# componentize-js spawns wizer with a synthesised env that strips HOME.
# wasmtime's cache config then can't find ProjectDirs and errors out.
# /tmp is universally writable. See compiler/js-engine/Dockerfile.
ENV HOME=/tmp \
    WM_JS_ENGINE_OUT=/out/js-engine.wasm \
    WM_JS_ENGINE_SRC=/app/engine.prebuilt.js

RUN mkdir -p /out && node build.mjs


# ── Stage 3: Rust builder ────────────────────────────────────────────────
FROM rust:1-bookworm AS rust-builder

# cmake: aws-lc-sys (via rustls). clang: some swc / wasmtime crates
# build C/C++ shims and prefer clang. pkg-config: standard build dep.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        cmake \
        clang \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# wasm-tools is invoked by build.rs to componentize the test fixtures.
# Even in --release we run the fixture compile (cargo can't tell at build
# time that fixtures are test-only). Pulling the prebuilt binary from
# the upstream release is ~30 s vs ~5 min for `cargo install --locked`.
# TARGETARCH is auto-injected by BuildKit (amd64|arm64); we translate
# to wasm-tools' release suffix (x86_64-linux|aarch64-linux).
ARG TARGETARCH
ARG WASM_TOOLS_VERSION=1.248.0
RUN case "$TARGETARCH" in \
        amd64) WT_ARCH="x86_64-linux" ;; \
        arm64) WT_ARCH="aarch64-linux" ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac && \
    curl -fsSL \
        "https://github.com/bytecodealliance/wasm-tools/releases/download/v${WASM_TOOLS_VERSION}/wasm-tools-${WASM_TOOLS_VERSION}-${WT_ARCH}.tar.gz" \
        | tar -xz -C /tmp && \
    install -m 0755 "/tmp/wasm-tools-${WASM_TOOLS_VERSION}-${WT_ARCH}/wasm-tools" /usr/local/bin/wasm-tools && \
    rm -rf "/tmp/wasm-tools-${WASM_TOOLS_VERSION}-${WT_ARCH}"

WORKDIR /src

# ── Dependency layer ─────────────────────────────────────────────────────
#
# Build the third-party dependency tree from manifests alone, before any
# source is copied in, so this layer's cache key is Cargo.lock + the four
# crate manifests rather than "any file in the repo". A push that doesn't
# touch dependencies reuses it.
#
# This deliberately does NOT use `--mount=type=cache`. Cache mounts live on
# the builder, and `cache-to: type=gha` does not export them — so on CI,
# where every run gets a fresh runner and a fresh builder, they start empty
# and the whole release dependency tree was being recompiled on every push
# (~14 min of the ~21 min CI wall clock). Writing the artifacts into a real
# layer is what makes them survive, because layers are what the GHA cache
# actually stores.
#
# The stubs are the minimum that satisfies each manifest's declared targets:
# a lib for the libraries, a main for the binaries, and a no-op build.rs for
# wm-host so its [build-dependencies] (wm-transpile → swc, the single most
# expensive subtree) are compiled here too. Adding a workspace member
# without adding it here fails loudly at this step rather than silently
# skipping the cache.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/wm-core/Cargo.toml crates/wm-core/
COPY crates/wm-host/Cargo.toml crates/wm-host/
COPY crates/wm-cli/Cargo.toml crates/wm-cli/
COPY crates/wm-transpile/Cargo.toml crates/wm-transpile/
RUN mkdir -p crates/wm-core/src crates/wm-host/src crates/wm-cli/src crates/wm-transpile/src && \
    : > crates/wm-core/src/lib.rs && \
    : > crates/wm-host/src/lib.rs && \
    : > crates/wm-transpile/src/lib.rs && \
    echo 'fn main() {}' > crates/wm-host/src/main.rs && \
    echo 'fn main() {}' > crates/wm-cli/src/main.rs && \
    echo 'fn main() {}' > crates/wm-host/build.rs && \
    cargo build --release -p wm-host --bin wm-host && \
    rm -rf crates

# Whole repo copy — .dockerignore controls what gets pulled in (no
# target/, no node_modules, no .git).
COPY . .

# build.rs componentizes the fixtures via `cargo build --target
# wasm32-unknown-unknown`, so the target must be installed even though
# the bin we ship is x86_64-linux. Add it *after* the COPY so we hit
# whichever toolchain rust-toolchain.toml selects (rustup auto-installs
# the channel pin on first cargo invocation; this ensures the target is
# attached to that toolchain, not whatever shipped with the base image).
RUN rustup show && rustup target add wasm32-unknown-unknown

# Hand-off from stage 1. build.rs honors WM_JS_ENGINE_WASM_OVERRIDE and
# skips its own docker run when this is set.
COPY --from=js-engine-builder /out/js-engine.wasm /tmp/js-engine.wasm
ENV WM_JS_ENGINE_WASM_OVERRIDE=/tmp/js-engine.wasm

# Real build, reusing the dependency layer's target/ and registry.
#
# `touch` first: cargo fingerprints on mtime, and the stub sources compiled
# above can end up newer than the real ones COPYed over them (COPY preserves
# the context's mtimes). Without this cargo can consider the stub build
# fresh and ship an empty binary — a silent, shipped-artifact failure, so it
# is asserted below rather than trusted.
RUN find crates -name '*.rs' -exec touch {} + && \
    cargo build --release -p wm-host --bin wm-host && \
    cp /src/target/release/wm-host /usr/local/bin/wm-host && \
    grep -q "wm-host listening" /usr/local/bin/wm-host


# ── Stage 4: runtime ─────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates: outbound TLS (rediss://, OIDC providers).
# libgcc-s1: linked dynamically by libstd.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* && \
    useradd \
        --system \
        --uid 10001 \
        --user-group \
        --no-create-home \
        --shell /usr/sbin/nologin \
        wm

COPY --from=rust-builder /usr/local/bin/wm-host /usr/local/bin/wm-host

USER wm
EXPOSE 8080

# Bind all interfaces by default so `docker run -p 8080:8080` is
# reachable. Deployments behind a reverse proxy on the same host
# (e.g. Caddy via host networking) can override to 127.0.0.1:8080.
ENV WM_LISTEN_ADDR=0.0.0.0:8080

ENTRYPOINT ["/usr/local/bin/wm-host"]
