# syntax=docker/dockerfile:1.7

# Build stage
FROM rust:1-bookworm AS builder
WORKDIR /usr/src/revolver

# Cache dependencies first by building against a dummy main.rs.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/revolver target/release/deps/revolver-*

# Build the actual binary.
COPY src ./src
COPY tests ./tests
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim AS runtime

# ca-certificates only — SQLite is bundled, reqwest is built without TLS,
# and tag reading does not need any system libraries.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/revolver/target/release/revolver /usr/local/bin/revolver

# SSDP requires host networking; these EXPOSE lines are documentation only.
EXPOSE 8200/tcp
EXPOSE 1900/udp

# Expected mount points; override at run time with -v.
VOLUME ["/music", "/data"]

WORKDIR /data
ENTRYPOINT ["/usr/local/bin/revolver"]
CMD ["--config", "/data/config.toml"]
