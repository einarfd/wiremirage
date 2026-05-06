# Multi-stage build for the WireMirage TypeScript compiler sidecar.
#
# The WIT directory is copied in at build time; componentize-js resolves
# the world definitions against this path. To rebuild against a newer
# WIT, rebuild the image — the WIT is intentionally pinned so a sidecar
# can't drift out of sync with the contract it advertises.

FROM node:22-slim AS builder

WORKDIR /app

# Install deps using the lockfile for reproducibility.
COPY compiler/typescript/package.json compiler/typescript/package-lock.json ./
RUN npm ci

# Copy source and build.
COPY compiler/typescript/tsconfig.json ./
COPY compiler/typescript/src ./src
COPY compiler/typescript/sdk ./sdk
RUN npm run build

# Drop dev deps to slim the runtime image.
RUN npm prune --omit=dev

# -----------------------------------------------------------------------------

FROM node:22-slim

WORKDIR /app

# Copy the built service + production node_modules.
COPY --from=builder /app/dist ./dist
COPY --from=builder /app/node_modules ./node_modules
COPY --from=builder /app/package.json ./
COPY --from=builder /app/sdk ./sdk

# WIT contract — pinned at image build time.
COPY wit /app/wit

ENV WM_HANDLER_WIT_PATH=/app/wit
ENV WM_COMPILER_PORT=9100
EXPOSE 9100

# StarlingMonkey + componentize-js need a few tens of MB of heap during
# componentization; default Node heap is plenty.
CMD ["node", "dist/server.js"]
