# syntax=docker/dockerfile:1.7

FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.6.1 AS xx

FROM --platform=$BUILDPLATFORM rust:1.97.1-bookworm AS rust-base
COPY --from=xx / /
ENV RUSTUP_TOOLCHAIN=1.97.1 \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0
WORKDIR /src

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates clang lld \
    && cargo install cargo-chef --locked --version 0.1.78 \
    && rm -rf /var/lib/apt/lists/*

FROM rust-base AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM rust-base AS core-builder
ARG TARGETPLATFORM
RUN xx-apt-get install -y --no-install-recommends gcc g++ libc6-dev
COPY --from=planner /src/recipe.json ./
RUN triple="$(xx-cargo --print-target-triple)" \
    && rustup target add "$triple" \
    && xx-cargo chef cook --release --target "$triple" --recipe-path recipe.json
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN triple="$(xx-cargo --print-target-triple)" \
    && xx-cargo build --release --locked --target "$triple" -p provider-server \
    && xx-verify "/src/target/$triple/release/provider-core" \
    && install -Dm755 "/src/target/$triple/release/provider-core" /tmp/provider-core

FROM debian:bookworm-slim AS core-runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /app --shell /usr/sbin/nologin provider \
    && mkdir -p /app/data \
    && chown -R provider:provider /app

COPY --from=core-builder /tmp/provider-core /usr/local/bin/provider-core

USER provider
ENV LISTEN_ADDRESS=0.0.0.0:8317
EXPOSE 8317
VOLUME ["/app/data"]

HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=5 \
  CMD curl -fsS http://127.0.0.1:8317/livez >/dev/null

CMD ["provider-core"]

FROM --platform=$BUILDPLATFORM scratch AS ui-source
ARG UI_REF=main
ADD https://github.com/ai-boxes/provider-ui.git#${UI_REF} /ui

FROM --platform=$BUILDPLATFORM node:24-bookworm-slim AS ui-builder
WORKDIR /ui

COPY --from=ui-source /ui/package.json /ui/package-lock.json ./
RUN npm ci --no-audit --no-fund

COPY --from=ui-source /ui/ ./
RUN npm run build

FROM core-runtime AS runtime
COPY --from=ui-builder --chown=provider:provider /ui/dist /app/public
