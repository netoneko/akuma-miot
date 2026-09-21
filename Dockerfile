# Build and run the litter simulation on Linux.
#
# Nothing exotic: there is no wasm toolchain, no wasm32 target, no C++ database
# and no `sc-executor` in this image, because the runtime is executed natively
# (see crates/miot-runtime/src/lib.rs). That is the whole reason this Dockerfile
# is five lines of apt instead of a substrate build environment.
FROM rust:1.98-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential clang pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY assets ./assets
RUN cargo build --release -p miot-sim

FROM debian:bookworm-slim
COPY --from=build /src/target/release/miot-sim /usr/local/bin/miot-sim
ENTRYPOINT ["/usr/local/bin/miot-sim"]
