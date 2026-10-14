# mammoth-cache

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="branding/logo-dark.svg">
  <img alt="mammoth-cache" src="branding/logo.svg" width="320">
</picture>

mammoth-cache is a fast, open-source **remote build cache server for Gradle and Maven**, written in
Rust. It is a self-hosted, drop-in alternative to the Gradle Build Cache Node, which reaches end of
life after December 31, 2026: same `/cache/` endpoint, same port, no Develocity needed. It serves
Gradle's HTTP build cache and the Apache Maven Build Cache Extension, and is designed to keep
entries in memory, on local disk and in an optional S3, GCS or Azure bucket, with Prometheus
metrics.

Pre-alpha: today it serves Gradle and Maven from memory; the disk and bucket tiers, config file and
authentication are on the way. Try it with the `edge` image
(`ghcr.io/nevzheng/mammoth-cache:edge`), but don't run it in production yet.

## Documentation

Read the docs at <https://nevzheng.github.io/mammoth-cache/>. Sources live in
[docs/](docs/README.md): the [user guide](docs/user/README.md) and
[design docs](docs/design/README.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
