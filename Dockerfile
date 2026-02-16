# Build stage
FROM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r meddler && useradd -r -g meddler -s /sbin/nologin meddler

COPY --from=builder /app/target/release/meddler /usr/local/bin/meddler

EXPOSE 8080 9090

USER meddler

ENTRYPOINT ["meddler"]
CMD ["--config", "/etc/meddler/config.yaml"]
