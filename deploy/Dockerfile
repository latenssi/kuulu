# kuulu-server image. Build from the repository root:
#   docker build -f deploy/Dockerfile -t kuulu-server .
# Multi-arch (cross-compiles; no emulation needed):
#   docker buildx build -f deploy/Dockerfile --platform linux/amd64,linux/arm64 .
#
# Base images are pinned by digest; bump tag and digest together.

# Runs on the build machine's architecture and cross-compiles for the target.
FROM --platform=$BUILDPLATFORM rust:1.99.1-slim-bookworm@sha256:86234e857a413004348b471d11a4ebe1aa92d2eae2b6b143cac68fd1ac3c48be AS build
ARG TARGETARCH
# protoc for prost-build; a C cross compiler for ring and bundled SQLite.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
    gcc-aarch64-linux-gnu libc6-dev-arm64-cross gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
 && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu ;; \
      arm64) echo aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac > /target \
 && rustup target add "$(cat /target)"
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CARGO_PROFILE_RELEASE_STRIP=symbols
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=kuulu-registry-$TARGETARCH \
    --mount=type=cache,target=/src/target,id=kuulu-target-$TARGETARCH \
    cargo build --locked --release -p kuulu-server --target "$(cat /target)" \
 && cp "target/$(cat /target)/release/kuulu-server" /kuulu-server \
 && mkdir /data

# Only the binary, for attaching to releases:
#   docker buildx build -f deploy/Dockerfile --target binary --output out .
FROM scratch AS binary
COPY --from=build /kuulu-server /

# distroless/cc: glibc, libgcc and CA certificates, no shell or package
# manager. The binary links glibc dynamically (built against Debian 12).
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /kuulu-server /usr/local/bin/kuulu-server
# Owned by the nonroot user (65532) so a new named volume is writable.
COPY --from=build --chown=65532:65532 /data /data
WORKDIR /data
VOLUME /data
EXPOSE 7700/udp
# The data directory defaults to the working directory, so
# `docker exec <container> kuulu-server invite` needs no path.
ENTRYPOINT ["/usr/local/bin/kuulu-server"]
