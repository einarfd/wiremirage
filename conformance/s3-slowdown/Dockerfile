# Conformance client: the real AWS Go SDK (aws-sdk-go-v2). Run by ../run.sh
# against a wm-host on the host network (--network host), WM_BASE pointing at
# it. Pinned base image + go.mod/go.sum so the lane is reproducible.
FROM golang:1.26

WORKDIR /app
COPY go.mod go.sum ./
RUN go mod download
COPY main.go ./
RUN go build -o /conformance .

ENV WM_BASE=http://localhost:8080
CMD ["/conformance"]
