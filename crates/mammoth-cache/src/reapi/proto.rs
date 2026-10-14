//! Messages, servers and clients generated from the vendored REAPI v2 and Google `ByteStream`
//! protos in `third_party/proto`. Regenerate with
//! `cargo run --manifest-path scripts/reapi-codegen/Cargo.toml`; never edit the files by hand.

#![allow(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    rust_2018_idioms,
    clippy::all,
    clippy::pedantic
)]

pub mod build {
    pub mod bazel {
        pub mod remote {
            pub mod execution {
                pub mod v2 {
                    include!("proto/build.bazel.remote.execution.v2.rs");
                }
            }
        }
        pub mod semver {
            include!("proto/build.bazel.semver.rs");
        }
    }
}

pub mod google {
    pub mod api {
        include!("proto/google.api.rs");
    }
    pub mod bytestream {
        include!("proto/google.bytestream.rs");
    }
    pub mod longrunning {
        include!("proto/google.longrunning.rs");
    }
    /// `google.rpc` comes from tonic-types, which builds and reads its details.
    pub use tonic_types::pb as rpc;
}
