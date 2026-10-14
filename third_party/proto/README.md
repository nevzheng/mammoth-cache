# Vendored protos

The REAPI cache adapter (`crates/mammoth-cache/src/reapi`, design doc 0016) is generated from these
files. They are copied unchanged from:

| Files                                 | Source                                                                               | Licence                                    |
| ------------------------------------- | ------------------------------------------------------------------------------------ | ------------------------------------------ |
| `build/bazel/**`                      | [bazelbuild/remote-apis](https://github.com/bazelbuild/remote-apis) `v2.12.0`        | Apache-2.0, [LICENSE](LICENSE-remote-apis) |
| `google/**`                           | [googleapis/googleapis](https://github.com/googleapis/googleapis) `b6efa15f5537f053` | Apache-2.0, [LICENSE](LICENSE-googleapis)  |

To update, replace the files and regenerate the Rust code, then commit both:

```sh
cargo run --manifest-path scripts/reapi-codegen/Cargo.toml
```

The generator uses protox, so no `protoc` is needed.
