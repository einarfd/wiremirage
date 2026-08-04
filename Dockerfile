# Multi-stage Dockerfile for the WireMirage host.
#
# Stage 1 (js-engine-builder) builds the shared js-engine.wasm via
# componentize-js, mirroring compiler/js-engine/Dockerfile. We don't
# reuse that Dockerfile directly because it would require docker-in-
# docker; instead we inline the same steps as a build stage and pass
# the output to the Rust builder via WM_JS_ENGINE_WASM_OVERRIDE so
# build.rs skips its own docker invocation.
#
# Stage 2 (rust-builder) compiles wm-host in release mode.
#
# Stage 3 (runtime) ships the binary on debian:bookworm-slim with
# ca-certificates and libgcc only.

# syntax=docker/dockerfile:1.7


# ── Stage 1: js-engine builder ───────────────────────────────────────────
FROM node:26-bookworm-slim@sha256:9e6f9357d371591e32ab6f2d8a26d63bdd0d17c29eee3f4f3e7e454d9634bf73 AS js-engine-builder

WORKDIR /app

# npm install layer first so engine source edits don't bust the install cache.
COPY compiler/js-engine/package.json compiler/js-engine/package-lock.json ./
RUN npm ci --silent

COPY compiler/js-engine/src ./src
COPY compiler/js-engine/wit ./wit
COPY compiler/js-engine/build.mjs ./

# componentize-js spawns wizer with a synthesised env that strips HOME.
# wasmtime's cache config then can't find ProjectDirs and errors out.
# /tmp is universally writable. See compiler/js-engine/Dockerfile.
ENV HOME=/tmp \
    WM_JS_ENGINE_OUT=/out/js-engine.wasm

RUN mkdir -p /out && node build.mjs


# ── Stage 2: Rust builder ────────────────────────────────────────────────
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

# Whole repo copy — .dockerignore controls what gets pulled in (no
# target/, no node_modules, no .git). For workspaces with N crates, the
# usual "Cargo.toml-only pre-build" trick is awkward; cargo's BuildKit
# cache mount below keeps incremental builds fast on a warm builder.
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

# Cache mounts persist /src/target and the cargo registry across builds
# (per-builder, not in the image). The `cp` copies the binary out of the
# cache mount and into a regular layer so the next stage can COPY it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p wm-host --bin wm-host && \
    cp /src/target/release/wm-host /usr/local/bin/wm-host


# ── Stage 3: runtime ─────────────────────────────────────────────────────
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
