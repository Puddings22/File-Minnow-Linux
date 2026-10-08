# syntax=docker/dockerfile:1
# Release build. Ubuntu 22.04 (glibc 2.35, GTK 3.24.33) is the oldest
# baseline: the binary then runs on Linux Mint 21+, Ubuntu 22.04+, Debian 12+,
# Fedora 36+ and similar. The Rust toolchain is copied from the pinned
# official image, so nothing is piped from the network into a shell.
FROM rust:1.96.0-slim-bullseye@sha256:290a6e49a53e54df34abc66be10c77af6e446b88f23dd62ea5b1793ab04e040c AS rust

FROM ubuntu:22.04@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401 AS build
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       ca-certificates gcc libc6-dev pkg-config libgtk-3-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Window and tray icons are embedded at build time.
COPY packaging/icons ./packaging/icons
ENV CARGO_BUILD_JOBS=4
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin file-minnow \
    && mkdir /out && cp target/release/file-minnow /out/file-minnow \
    && rustc --version > /out/toolchain.txt \
    && ldd --version | head -1 >> /out/toolchain.txt \
    && pkg-config --modversion gtk+-3.0 | sed 's/^/gtk /' >> /out/toolchain.txt

FROM scratch AS export
COPY --from=build /out/ /
