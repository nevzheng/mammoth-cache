# syntax=docker/dockerfile:1

# Bootstrap image: a release build of the `mammoth` binary on distroless. To be developed further.

# Keep the Rust version in step with rust-toolchain.toml.
FROM rust:1.98.1-slim-trixie AS builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin mammoth \
    && cp target/release/mammoth /mammoth

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /mammoth /mammoth
ENV MAMMOTH_ADDR=0.0.0.0:5071
EXPOSE 5071
USER nonroot
ENTRYPOINT ["/mammoth"]
