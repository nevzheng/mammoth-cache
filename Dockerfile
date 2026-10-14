# syntax=docker/dockerfile:1

# From-source image: a release build of the `mammoth` binary on distroless. Official images are
# built by Dockerfile.release. To build your own feature set, pass Cargo flags, for example:
#   docker build --build-arg CARGO_ARGS="--no-default-features --features jemalloc" .

# Keep the Rust version in step with rust-toolchain.toml.
FROM rust:1.98.1-slim-trixie AS builder
ARG CARGO_ARGS=""
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin mammoth ${CARGO_ARGS} \
    && cp target/release/mammoth /mammoth

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
COPY --from=builder /mammoth /mammoth
EXPOSE 5071
USER nonroot
ENTRYPOINT ["/mammoth"]
