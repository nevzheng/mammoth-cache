//! mammoth-cache: a remote build cache node for Gradle.
//!
//! Pre-alpha. The code follows a hexagonal structure (design doc 0003):
//!
//! - [`store`]: the blob store port, implemented by each storage tier (memory, disk, bucket).
//! - [`cache`]: the core, which applies the tiering rules over the port.
//! - [`http`]: inbound adapters for the Gradle protocol, health checks and the admin API.
//!
//! The crate is a library with a thin `mammoth` binary, so a custom build can supply its own
//! [`store::BlobStore`] implementation without forking.

pub mod cache;
pub mod http;
pub mod key;
pub mod store;

pub use http::router;
