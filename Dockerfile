# noetl-doctor image.
#
# Builds the Rust `noetl-doctor` binary, then bundles the upstream
# `noetl` Rust CLI release for `noetl run --runtime local`. The final
# image is an Ubuntu runtime suitable for either:
#
#   * a one-shot Kubernetes Job  → CMD: ["detect"]
#   * a long-running MCP server  → CMD: ["mcp", "serve"]
#
# The same image satisfies both roles; pick the role at deploy time by
# overriding `CMD`.
#
# Build args:
#   NOETL_CLI_VERSION   (default 2.14.2) — release tag of `noetl/cli`
#   TARGETARCH          (provided by Docker/Podman buildx: amd64 / arm64)

# ---- chef stage (cargo-chef dependency cache) -------------------------------
FROM lukemathwalker/cargo-chef:0.1.73-rust-1.91.1-alpine3.22 AS chef
WORKDIR /app
RUN apk add --no-cache clang lld llvm musl-dev make pkgconfig openssl-dev openssl-libs-static g++ libc-dev

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin noetl-doctor

# ---- noetl CLI fetch (uses release asset) -----------------------------------
FROM ubuntu:24.04 AS noetl-cli
ARG NOETL_CLI_VERSION=2.14.2
ARG TARGETARCH
WORKDIR /tmp
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tar \
 && rm -rf /var/lib/apt/lists/* \
 && case "${TARGETARCH}" in \
      amd64) NOETL_CLI_ARCH=x86_64 ;; \
      arm64) NOETL_CLI_ARCH=aarch64 ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/noetl.tar.gz \
        "https://github.com/noetl/cli/releases/download/v${NOETL_CLI_VERSION}/noetl-v${NOETL_CLI_VERSION}-linux-${NOETL_CLI_ARCH}.tar.gz" \
 && tar -xzf /tmp/noetl.tar.gz -C /tmp/ \
 && install -m 0755 /tmp/noetl /usr/local/bin/noetl \
 && /usr/local/bin/noetl --version

# ---- runtime ---------------------------------------------------------------
FROM ubuntu:24.04 AS runtime
# ===
LABEL org.opencontainers.image.source=https://github.com/noetl/doctor
LABEL org.opencontainers.image.licenses=MIT

WORKDIR /app
# Healing playbooks run under `noetl run --runtime local`, which only
# supports `kind: shell` / http / playbook / duckdb / auth / sink / rhai
# in the Rust CLI. Every doctor playbook is `kind: shell` driving
# `psql` / `curl` / `jq`, so the runtime image bundles those.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates bash curl jq postgresql-client wget \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder    /app/target/release/noetl-doctor /usr/local/bin/noetl-doctor
COPY --from=noetl-cli  /usr/local/bin/noetl              /usr/local/bin/noetl
COPY playbooks /app/playbooks

ENV NOETL_DOCTOR_NOETL_BIN=/usr/local/bin/noetl

EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8765/healthz >/dev/null 2>&1 || exit 1

ENTRYPOINT ["/usr/local/bin/noetl-doctor"]
CMD ["detect"]
