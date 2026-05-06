FROM rust:1-bookworm AS builder

LABEL org.opencontainers.image.source=https://github.com/fantasy194/micam-rs.git

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && \
    cp /app/target/release/micam-rs /usr/local/bin/micam-rs

FROM debian:bookworm-slim

ENV RUST_LOG=info

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates ffmpeg libssl3 && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/micam-rs /usr/local/bin/micam-rs

CMD ["micam-rs"]

