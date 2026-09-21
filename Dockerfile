# Build and run the litter on Linux.
#
# No wasm toolchain, no wasm32 target, no C++ database, no `sc-executor` —
# the runtime is executed natively (crates/miot-runtime/src/lib.rs). That is
# why this is three apt packages rather than a substrate build environment.
#
# Both stages pin bookworm ON PURPOSE. A newer builder links against a glibc
# the runtime image does not have, and the failure is at exec time, not build
# time: `libc.so.6: version GLIBC_2.39 not found`.
FROM rust:1.98-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential clang pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY assets ./assets
RUN cargo build --release -p miot-sim

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/miot-sim /usr/local/bin/miot-sim
ENTRYPOINT ["/usr/local/bin/miot-sim"]
