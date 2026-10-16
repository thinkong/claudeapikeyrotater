# syntax=docker/dockerfile:1

# ---- build ---------------------------------------------------------------
FROM rust:1.97-slim-trixie AS build
WORKDIR /src

# Build dependencies first so source edits don't recompile them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY pricing.toml ./
COPY ui ui
COPY src src
# touch: COPY keeps old mtimes, which can make cargo skip the real build.
RUN touch src/main.rs \
    && cargo build --release --locked \
    && mkdir -p /data

# ---- binary only (for release assets) --------------------------------------
# docker buildx build --target binary --output type=local,dest=dist .
FROM scratch AS binary
COPY --from=build /src/target/release/claude-key-router /claude-key-router

# ---- runtime (default target) ----------------------------------------------
# Distroless: glibc + CA certs only, no shell, runs as uid 65532 (nonroot).
FROM gcr.io/distroless/cc-debian13:nonroot

COPY --from=build /src/target/release/claude-key-router /usr/local/bin/claude-key-router
COPY --from=build --chown=65532:65532 /data /data

ENV CKR_LISTEN=0.0.0.0:8787 \
    CKR_DATABASE=/data/claude-key-router.db
WORKDIR /data
VOLUME /data
EXPOSE 8787

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/claude-key-router", "healthcheck"]

# No argument: uses /data/config.toml if present, otherwise defaults + CKR_* env vars.
# Pass a path to use a mounted config file instead.
ENTRYPOINT ["/usr/local/bin/claude-key-router"]
