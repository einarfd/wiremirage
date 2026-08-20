FROM node:24-bookworm-slim@sha256:3638d9a6fe4030bd716be989438248074489337ba3275657f93595428be4fc03
WORKDIR /app
COPY package.json ./
RUN npm install --silent
COPY src ./src
COPY wit ./wit
COPY build.mjs ./
ENV HOME=/tmp
CMD ["node", "build.mjs"]
