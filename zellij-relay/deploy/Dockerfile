# Multi-stage build for the zellij-relay binary.
#
# Build context is the workspace root (see compose.yml: build.context = ../..)
# so cargo can resolve the workspace correctly.

FROM rust:1.92-bookworm AS builder

WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/workspace/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build -p zellij-relay --release --locked && \
    cp /workspace/target/release/zellij-relay /usr/local/bin/zellij-relay

FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/zellij-relay /usr/local/bin/zellij-relay

ENV RELAY_BIND_ADDR=0.0.0.0:8765 \
    RUST_LOG=info

EXPOSE 8765

ENTRYPOINT ["/usr/local/bin/zellij-relay"]
