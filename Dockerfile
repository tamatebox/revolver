# syntax=docker/dockerfile:1.7

# Build stage
FROM rust:1-bookworm AS builder
WORKDIR /usr/src/revolver

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

# Cache the cargo registry and target dir across builds so a source-only
# rebuild only recompiles changed modules instead of the whole dep graph.
# target/ lives in a cache mount (not committed to the layer), so copy the
# finished binary out to a plain path for the runtime stage to grab.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/revolver/target \
    cargo build --release && \
    cp target/release/revolver /usr/local/bin/revolver

# Runtime stage — distroless: glibc + ca-certificates only, no shell / apt /
# perl, so the OS-package CVE surface shrinks to ~zero vs debian:bookworm-slim.
# Matches the bookworm (glibc 2.36) toolchain the binary is built against.
# SQLite is bundled, reqwest is built without TLS, tag reading needs no libs.
FROM gcr.io/distroless/cc-debian12 AS runtime

COPY --from=builder /usr/local/bin/revolver /usr/local/bin/revolver

# SSDP requires host networking; these EXPOSE lines are documentation only.
EXPOSE 8200/tcp
EXPOSE 1900/udp

# Expected mount points; override at run time with -v.
VOLUME ["/music", "/data"]

WORKDIR /data
ENTRYPOINT ["/usr/local/bin/revolver"]
CMD ["--config", "/data/config.toml"]
