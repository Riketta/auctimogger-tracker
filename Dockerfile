# Multi-stage build: pinned stable toolchain (rust-toolchain.toml) for the
# build, slim Debian for the runtime. The gnu-target binary is statically
# linked against the image's glibc — unlike the release tarball, the image is
# immune to host glibc versions.

# syntax=docker/dockerfile:1
FROM rust:1-slim AS build
WORKDIR /build
# Whole-context copy: the workspace spans crates/* + Cargo.lock + the
# toolchain pin (see .dockerignore for what stays out).
COPY . .
# Cache mounts keep incremental rebuilds fast; the finished binary is copied
# out of the cache-mounted target dir onto the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p auctimogger-tracker \
    && cp target/release/auctimogger-tracker /build/auctimogger-tracker

FROM debian:bookworm-slim
# rustls ships its own root store (webpki-roots), but system CA certs are
# installed anyway so a future native-certs flip cannot silently break TLS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime. /work is the working directory: the serve mode writes the
# published feeds to ./feeds (relative path by design), the config and the
# data volume are mounted beside it (see docker-compose.yml).
RUN useradd --system --create-home --home-dir /work amtracker
WORKDIR /work
COPY --from=build /build/auctimogger-tracker /usr/local/bin/auctimogger-tracker
RUN mkdir -p /work/feeds /data && chown -R amtracker:amtracker /work /data
USER amtracker

# SQLite database + raw archive live on /data, published feeds on /work/feeds
# (named volumes inherit these directories' ownership on first use).
VOLUME ["/data", "/work/feeds"]

ENTRYPOINT ["/usr/local/bin/auctimogger-tracker"]
CMD ["serve", "--config", "/work/config.toml"]
