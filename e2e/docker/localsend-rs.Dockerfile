# Build
FROM rust:1-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config cmake g++ make libdbus-1-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# `localsend-rs` is a standalone vendor crate, but its optional protected
# LocalSend API now takes the public File-service request by value. Keep the
# container context small: copy only the declared local dependency closure,
# not the entire CrossCopy workspace or any build outputs.
COPY vendors/localsend-rs/Cargo.lock ./Cargo.lock
RUN printf '%s\n' \
  '[workspace]' \
  'resolver = "2"' \
  'members = [' \
  '  "vendors/localsend-rs",' \
  '  "packages/crosscopy-file-service",' \
  '  "packages/crosscopy-ipc",' \
  '  "packages/crosscopy-service",' \
  '  "packages/crosscopy-content",' \
  '  "packages/crosscopy-authorization",' \
  '  "packages/crosscopy-db",' \
  '  "packages/crosscopy-fabric",' \
  '  "packages/crosscopy-profile",' \
  '  "packages/crosscopy-clipboard",' \
  '  "packages/crosscopy-enrichment",' \
  ']' \
  '' \
  '[workspace.package]' \
  'version = "0.1.0"' \
  'edition = "2024"' \
  'license = "MIT"' \
  'repository = "https://github.com/CrossCopy/CrossCopy"' \
  > Cargo.toml
COPY packages/crosscopy-file-service ./packages/crosscopy-file-service
COPY packages/crosscopy-ipc ./packages/crosscopy-ipc
COPY packages/crosscopy-service ./packages/crosscopy-service
COPY packages/crosscopy-content ./packages/crosscopy-content
COPY packages/crosscopy-authorization ./packages/crosscopy-authorization
COPY packages/crosscopy-db ./packages/crosscopy-db
COPY packages/crosscopy-fabric ./packages/crosscopy-fabric
COPY packages/crosscopy-profile ./packages/crosscopy-profile
COPY packages/crosscopy-clipboard ./packages/crosscopy-clipboard
COPY packages/crosscopy-enrichment ./packages/crosscopy-enrichment
COPY packages/db-schema ./packages/db-schema
COPY vendors/localsend-rs ./vendors/localsend-rs
WORKDIR /src/vendors/localsend-rs
RUN cargo build --release --features cli,https

# Runtime
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/localsend-rs /usr/local/bin/localsend-rs
