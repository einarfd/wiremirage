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
FROM node:26-bookworm-slim@sha256:9e6f9357d371591e32ab6f2d8a26d63bdd0d17c29eee3f4f3e7e454d9634bf73

WORKDIR /app

# npm ci first so the (large) componentize-js install is cached as its own
# layer; engine.ts edits don't bust it.
COPY package.json package-lock.json ./
RUN npm ci --silent

COPY src ./src
COPY wit ./wit
COPY build.mjs ./

ENV WM_JS_ENGINE_OUT=/out/js-engine.wasm

# build.rs runs us as `--user $UID:$GID` so the output is host-owned.
# If that UID isn't in the image's /etc/passwd (e.g. GitHub Actions
# runners run as uid 1001, which isn't `node`), HOME ends up unset
# and wasmtime's `ProjectDirs::from` returns None — wizer (called by
# componentize-js) then errors with "config file not specified and
# failed to get the default". /tmp is writable for any uid; pinning
# HOME here makes the build independent of which uid we run as.
ENV HOME=/tmp

CMD ["node", "build.mjs"]
