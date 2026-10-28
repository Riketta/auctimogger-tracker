# Multi-stage build: pinned stable toolchain (rust-toolchain.toml) for the
# build, slim Debian for the runtime. The gnu-target binary is statically
# linked against the image's glibc — unlike the release tarball, the image is
# immune to host glibc versions.

# syntax=docker/dockerfile:1

# ---- build stage ----
# The -bookworm suffix pins the Debian suite, not just the compiler: the
# binary links against this base's glibc, and the runtime stage below is
# bookworm-slim. A moving default base could break the image at startup
# ("GLIBC_x.y not found"), not at build time (versa-bot precedent). The
# compiler itself follows the repo's rust-toolchain.toml pin (stable).
FROM rust:1-bookworm AS build
WORKDIR /build
# Whole-context copy: the workspace spans crates/* + Cargo.lock + the
# toolchain pin (see .dockerignore for what stays out).
COPY . .
# Cache mounts keep the cargo registry/git checkouts and the build tree warm
# between runs; the finished binary is copied out of the cache-mounted target
# dir onto a real layer (files written inside a cache mount do not land in
# the image layer).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p auctimogger-tracker \
    && cp target/release/auctimogger-tracker /build/auctimogger-tracker

# ---- runtime stage ----
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
# RUST_LOG is deliberately NOT baked into the image: an env var always wins
# over the config's stdout defaults (the top-level `debug` flag in
# config.docker.toml), so baking one in would silently disable that flag.
# EnvFilter falls back to "info" when unset; set RUST_LOG per-run via a
# compose `environment:` entry when directive-level control is wanted.

ENTRYPOINT ["/usr/local/bin/auctimogger-tracker"]
CMD ["serve", "--config", "/work/config.toml"]
