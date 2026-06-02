# syntax=docker/dockerfile:1

# Henka ships the language servers it drives, so the image bundles jdtls,
# rust-analyzer, and typescript-language-server alongside the binary, atop a
# runtime carrying the JRE and Node they need plus git and jj (Henka shells out
# to both for worktree/workspace detection). Compile-heavy and arch-independent
# stages pin to $BUILDPLATFORM to avoid emulation; only genuinely
# target-arch artifacts (the binary, rust-analyzer, jj) are built or fetched
# per target.

# ---- Stage 1: cross-compile the Henka binary via rustxc ----
# Henka has no native (C) dependencies, so it cross-compiles cleanly: stay on
# the build host's arch and target the requested one.
FROM --platform=$BUILDPLATFORM ghcr.io/martint/rustxc:latest AS backend
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    cargo build --release --package henka-server --target "$target"; \
    cp "/src/target/$target/release/henka" /henka

# ---- Stage 2: fetch jdtls and compile its delegate-command bundle ----
# jdtls is JVM bytecode and the bundle is compiled to bytecode, both
# arch-independent, so this runs on the build host. Needs a JDK (javac/jar).
FROM --platform=$BUILDPLATFORM eclipse-temurin:25-jdk-jammy AS jdtls
ARG JDTLS_VERSION=latest
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-jdtls.sh scripts/fetch-jdtls.sh
COPY jdtls-bundle jdtls-bundle
RUN JDTLS_VERSION="${JDTLS_VERSION}" bash scripts/fetch-jdtls.sh /opt/henka/jdtls

# ---- Stage 3: install typescript-language-server ----
# A Node/JS app, arch-independent; build on the host.
FROM --platform=$BUILDPLATFORM node:22-bookworm-slim AS typescript
WORKDIR /build
COPY scripts/fetch-typescript-language-server.sh scripts/fetch-typescript-language-server.sh
RUN bash scripts/fetch-typescript-language-server.sh /opt/henka/typescript-language-server

# ---- Stage 4: fetch the rust-analyzer binary (native, per target arch) ----
# Runs at the target platform so the fetch script's uname picks the right
# binary; it is only a download + gunzip, so emulation cost is negligible.
FROM debian:bookworm-slim AS rust-analyzer
ARG RUST_ANALYZER_VERSION=latest
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-rust-analyzer.sh scripts/fetch-rust-analyzer.sh
RUN RUST_ANALYZER_VERSION="${RUST_ANALYZER_VERSION}" \
    bash scripts/fetch-rust-analyzer.sh /opt/henka/rust-analyzer

# ---- Stage 5: fetch the matching jj CLI on the build host ----
FROM --platform=$BUILDPLATFORM curlimages/curl:8.10.1 AS jj-fetch
ARG TARGETARCH
ARG JJ_VERSION=0.41.0
WORKDIR /jj
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) jj_arch=x86_64 ;; \
      arm64) jj_arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
      "https://github.com/jj-vcs/jj/releases/download/v${JJ_VERSION}/jj-v${JJ_VERSION}-${jj_arch}-unknown-linux-musl.tar.gz" \
      | tar -xz

# ---- Stage 6: runtime ----
# A JRE (for jdtls) and Node (for typescript-language-server), git and jj for
# worktree/workspace detection, and the bundled servers. This is the release
# image; there is no leaner jj-less variant because Henka always needs jj.
# Ubuntu 24.04 (noble) for glibc 2.39 — the rustxc toolchain links the binary
# against it, so an older base (jammy / glibc 2.35) fails to load it at runtime.
FROM eclipse-temurin:25-jre-noble AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git \
 # Node, from NodeSource, for typescript-language-server.
 && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

COPY --from=backend /henka /usr/local/bin/henka
COPY --from=jj-fetch /jj/jj /usr/local/bin/jj
COPY --from=jdtls /opt/henka/jdtls /opt/henka/jdtls
COPY --from=jdtls /build/jdtls-bundle/henka-jdtls-bundle.jar /opt/henka/henka-jdtls-bundle.jar
COPY --from=rust-analyzer /opt/henka/rust-analyzer /opt/henka/rust-analyzer
COPY --from=typescript /opt/henka/typescript-language-server /opt/henka/typescript-language-server

# Point Henka at the bundled servers, and root all persistent state (the
# project registry and the per-repository indexes) under /data. HOME is a
# writable dir so the language servers and JVM have somewhere for incidental
# caches when the container runs as a non-root user.
ENV JDTLS_HOME=/opt/henka/jdtls \
    HENKA_JDTLS_BUNDLE=/opt/henka/henka-jdtls-bundle.jar \
    HENKA_RUST_ANALYZER=/opt/henka/rust-analyzer/rust-analyzer \
    HENKA_TYPESCRIPT_LANGUAGE_SERVER=/opt/henka/typescript-language-server/node_modules/.bin/typescript-language-server \
    HENKA_DATA=/data \
    HENKA_LOG=info \
    HOME=/home/henka

# Run the container as the host user (compose `user:` / `docker run --user`) so
# the files Henka writes — edits in the /workspaces bind mount, and its state
# under /data — are owned by that user, not root. That means an arbitrary uid
# must be able to write both the data dir (which seeds the named volume) and
# HOME, so make them world-writable here; the container is single-tenant.
RUN mkdir -p /data /home/henka && chmod 0777 /data /home/henka

# Settle the data dir on a well-known path so a bare `docker run` works without
# operator setup. Persist via `-v henka-data:/data`.
VOLUME ["/data"]
EXPOSE 8181
ENTRYPOINT ["henka"]
# Bind to every interface inside the container — loopback would be unreachable
# from outside. Host-side port mapping decides external reachability.
CMD ["--transport", "http", "--bind", "0.0.0.0:8181"]
