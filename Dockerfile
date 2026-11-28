# The manifest pins Rust 1.100.0 to match .github/workflows/check.yml.
FROM rust:1.100-slim-trixie@sha256:3b199fd6db7d709274df56565b8f4003f9e30d7bfcff9d53148c86053b7855e2 AS build

RUN apt-get update \
    && apt-get install -y cmake clang libclang-dev git xz-utils curl \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://ziglang.org/download/0.17.1/zig-x86_64-linux-0.17.1.tar.xz -o /tmp/zig.tar.xz \
    && echo '3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a  /tmp/zig.tar.xz' | sha256sum -c - \
    && mkdir /opt/zig \
    && tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 \
    && rm /tmp/zig.tar.xz
ENV PATH="/opt/zig:$PATH"

RUN cargo install --locked cargo-zigbuild@0.24.1 \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo zigbuild --locked --release --target x86_64-unknown-linux-musl

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends tini socat \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --home-dir /home/scour scour

COPY --from=build /build/target/x86_64-unknown-linux-musl/release/scour /usr/local/bin/scour
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/scour-entrypoint

EXPOSE 7268
USER scour
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/scour-entrypoint"]
CMD []

FROM runtime AS scour
