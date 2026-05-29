# ---- build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch so cargo rebuilds with the real sources.
RUN touch src/main.rs && cargo build --release

# ---- runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/mcp-icloud-calendar-rs /usr/local/bin/mcp-icloud-calendar-rs

ENV MCP_BIND=0.0.0.0:8000
EXPOSE 8000
USER 1000:1000

ENTRYPOINT ["/usr/local/bin/mcp-icloud-calendar-rs"]
