# Build
FROM rust:1-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config cmake g++ make libdbus-1-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --features cli,https

# Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/localsend-rs /usr/local/bin/localsend-rs
