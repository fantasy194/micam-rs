FROM rust:1-bookworm AS builder

LABEL org.opencontainers.image.source=https://github.com/fantasy194/micam-rs.git

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release && \
    cp /app/target/release/micam-rs /usr/local/bin/micam-rs && \
    cp /app/target/release/miloco_probe /usr/local/bin/miloco_probe && \
    cp /app/target/release/miloco_oauth_redirect /usr/local/bin/miloco_oauth_redirect && \
    cp /app/target/release/miot_native_probe /usr/local/bin/miot_native_probe

FROM debian:bookworm-slim

ENV RUST_LOG=info

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates ffmpeg libgcc-s1 libssl3 libstdc++6 && \
    rm -rf /var/lib/apt/lists/*

ADD https://raw.githubusercontent.com/XiaoMi/xiaomi-miloco/main/miot_kit/miot/libs/linux/x86_64/libmiot_camera_lite.so /usr/local/lib/libmiot_camera_lite.so
RUN chmod 0644 /usr/local/lib/libmiot_camera_lite.so

COPY --from=builder /usr/local/bin/micam-rs /usr/local/bin/micam-rs
COPY --from=builder /usr/local/bin/miloco_probe /usr/local/bin/miloco_probe
COPY --from=builder /usr/local/bin/miloco_oauth_redirect /usr/local/bin/miloco_oauth_redirect
COPY --from=builder /usr/local/bin/miot_native_probe /usr/local/bin/miot_native_probe

CMD ["micam-rs"]
