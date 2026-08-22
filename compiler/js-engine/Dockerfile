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
FROM node:24-bookworm-slim@sha256:3638d9a6fe4030bd716be989438248074489337ba3275657f93595428be4fc03

WORKDIR /app

# npm ci first so the (large) componentize-js install is cached as its own
# layer; engine.ts edits don't bust it.
COPY package.json package-lock.json ./
RUN npm ci --silent

COPY src ./src
COPY wit ./wit
COPY build.mjs tsconfig.json ./

# Type-check engine.ts (ADR-0038). Emit is swc's job in wm-host's
# build.rs; typescript is here purely as a checker, which is the one job
# the 7.x Go port is good at — its programmatic transpile API is gone,
# but the CLI is fine. Runs at image build so a type error fails
# `cargo build`, and the layer cache means it costs nothing on a rebuild
# where src/ hasn't changed.
RUN npm run typecheck

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
