# Build environment for the shared js-engine.wasm component (ADR-0020).
#
# Pinned by digest so the host's `build.rs` produces functionally identical
# output across machines. componentize-js needs glibc — node:22-alpine
# trips StarlingMonkey's Wizer init, so we stay on bookworm-slim.
#
# The host's build.rs builds this image (cached by docker layers) and runs
# it with the cargo OUT_DIR bind-mounted at /out, with the build script
# writing js-engine.wasm there. `--user $UID:$GID` keeps the output
# host-owned.
FROM node:22-bookworm-slim@sha256:7af03b14a13c8cdd38e45058fd957bf00a72bbe17feac43b1c15a689c029c732

WORKDIR /app

# npm ci first so the (large) componentize-js install is cached as its own
# layer; engine.ts edits don't bust it.
COPY package.json package-lock.json ./
RUN npm ci --silent

COPY src ./src
COPY wit ./wit
COPY build.mjs ./

ENV WM_JS_ENGINE_OUT=/out/js-engine.wasm

CMD ["node", "build.mjs"]
