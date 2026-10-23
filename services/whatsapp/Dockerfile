# =============================================================================
#  WhatsApp gateway
#    target tools   : toolchain for make format/lint/check-docs/test/bench (not shipped)
#    target runtime : static binary on distroless (what docker compose runs)
# =============================================================================
ARG GO_VERSION=1.27.2

# ---- tools -------------------------------------------------------------------
FROM golang:${GO_VERSION}-trixie AS tools
ARG GOLANGCI_LINT_VERSION=2.15.0
ARG BUF_VERSION=1.75.0
ARG PROTOC_GEN_GO_VERSION=v1.36.12
ARG PROTOC_GEN_GO_GRPC_VERSION=v1.6.2
ARG LIZARD_VERSION=1.24.2
RUN apt-get update \
 && apt-get install -y --no-install-recommends linux-perf python3-pip \
 && rm -rf /var/lib/apt/lists/* \
 && pip install --no-cache-dir --break-system-packages lizard==${LIZARD_VERSION}
RUN curl -fsSL "https://github.com/golangci/golangci-lint/releases/download/v${GOLANGCI_LINT_VERSION}/golangci-lint-${GOLANGCI_LINT_VERSION}-linux-amd64.tar.gz" \
     | tar -xz -C /usr/local/bin --strip-components=1 "golangci-lint-${GOLANGCI_LINT_VERSION}-linux-amd64/golangci-lint" \
 && curl -fsSL -o /usr/local/bin/buf "https://github.com/bufbuild/buf/releases/download/v${BUF_VERSION}/buf-Linux-x86_64" \
 && chmod +x /usr/local/bin/buf
RUN GOBIN=/usr/local/bin go install google.golang.org/protobuf/cmd/protoc-gen-go@${PROTOC_GEN_GO_VERSION} \
 && GOBIN=/usr/local/bin go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@${PROTOC_GEN_GO_GRPC_VERSION} \
 && GOBIN=/usr/local/bin go install golang.org/x/perf/cmd/benchstat@latest \
 && rm -rf /root/go /root/.cache
# Caches live in a per-user volume mounted at /cache (world-writable so any UID works).
RUN mkdir -m 1777 /cache
ENV GOCACHE=/cache/go-build GOMODCACHE=/cache/go-mod GOLANGCI_LINT_CACHE=/cache/golangci \
    HOME=/tmp GOFLAGS=-buildvcs=false GOTOOLCHAIN=local

# ---- build -------------------------------------------------------------------
FROM golang:${GO_VERSION}-trixie AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY main.go ./
COPY pb ./pb
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/whatsapp .

# ---- runtime -----------------------------------------------------------------
FROM gcr.io/distroless/static-debian12:nonroot AS runtime
COPY --from=build /out/whatsapp /whatsapp
EXPOSE 50051
USER nonroot:nonroot
HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=5 CMD ["/whatsapp", "-healthcheck"]
ENTRYPOINT ["/whatsapp"]
