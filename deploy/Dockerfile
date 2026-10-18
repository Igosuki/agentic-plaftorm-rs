# syntax=docker/dockerfile:1.7
FROM rust:1-slim-trixie AS builder
ARG WORKSPACE=.
ARG BIN
ARG CARGO_FEATURES=
# Each release rustc on rig/asupersync needs ~1-2 GB; unbounded jobs exhaust Docker Desktop's VM.
ARG CARGO_BUILD_JOBS=4
ENV RUST_MIN_STACK=1073741824 \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} \
    CARGO_TARGET_DIR=/target \
    CARGO_TERM_COLOR=never
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config gcc make libc6-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Separate layer so the pinned nightly is installed once, not per source change.
COPY rust-toolchain.toml .
RUN rustup show
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/target,id=target-${WORKSPACE},sharing=locked \
    test -n "${BIN}" \
 && cargo build --release --locked --manifest-path "${WORKSPACE}/Cargo.toml" -p "${BIN}" ${CARGO_FEATURES:+--features $CARGO_FEATURES} \
 && cp "/target/release/${BIN}" /app-bin

FROM debian:trixie-slim AS base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --create-home --home-dir /app app \
 && mkdir /data && chown app /data
USER app
WORKDIR /app
ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/app"]

# Packages host-built binaries (`make prebuilt`). rustc needs ~10 GB RSS for
# asupersync, more than Docker Desktop's default VM memory.
FROM base AS prebuilt
ARG BIN
COPY deploy/prebuilt/${BIN} /usr/local/bin/app

FROM base AS runtime
COPY --from=builder /app-bin /usr/local/bin/app
