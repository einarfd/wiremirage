# Conformance client: the real AWS Go SDK (aws-sdk-go-v2). Run by ../run.sh
# against a wm-host on the host network (--network host), WM_BASE pointing at
# it. Pinned base image + go.mod/go.sum so the lane is reproducible.
# TODO(digest-pin): match the js-engine Dockerfile convention and pin by
# digest so the weekly cron can't go red from base-image drift alone:
#   docker manifest inspect golang:1.27      # -> sha256:...
# FROM golang:1.27@sha256:<digest>
FROM golang:1.27

WORKDIR /app
COPY go.mod go.sum ./
RUN go mod download
COPY main.go ./
RUN go build -o /conformance .

ENV WM_BASE=http://localhost:8080
CMD ["/conformance"]
