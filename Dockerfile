# The manifest pins Rust 1.100.0 to match .github/workflows/check.yml.
FROM --platform=$BUILDPLATFORM rust:1.100-slim-trixie@sha256:3b199fd6db7d709274df56565b8f4003f9e30d7bfcff9d53148c86053b7855e2 AS build

RUN apt-get update \
    && apt-get install -y cmake clang libclang-dev git xz-utils curl \
    && rm -rf /var/lib/apt/lists/*

ARG BUILDARCH
RUN case "$BUILDARCH" in \
        amd64) zig=zig-x86_64-linux-0.17.1; sha=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a ;; \
        arm64) zig=zig-aarch64-linux-0.17.1; sha=a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795 ;; \
        *) echo "scour: no Zig build for $BUILDARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://ziglang.org/download/0.17.1/$zig.tar.xz" -o /tmp/zig.tar.xz \
    && echo "$sha  /tmp/zig.tar.xz" | sha256sum -c - \
    && mkdir /opt/zig \
    && tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 \
    && rm /tmp/zig.tar.xz
ENV PATH="/opt/zig:$PATH"

RUN cargo install --locked cargo-zigbuild@0.24.1 \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-musl ;; \
        arm64) target=aarch64-unknown-linux-musl ;; \
        *) echo "scour: no Rust target for $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && cargo zigbuild --locked --release --target "$target" \
    && cp "target/$target/release/scour" /build/scour

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends tini socat \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --home-dir /home/scour scour

COPY --from=build /build/scour /usr/local/bin/scour
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/scour-entrypoint

EXPOSE 7268
USER scour
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/scour-entrypoint"]
CMD []

FROM runtime AS scour
