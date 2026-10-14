//! mammoth-cache: a remote build cache node for Gradle.
//!
//! Pre-alpha. The code follows a hexagonal structure (design doc 0003):
//!
//! - [`store`]: the blob store port, implemented by each storage tier (memory, disk, bucket).
//! - [`cache`]: the core, which applies the tiering rules over the port.
//! - [`config`]: the `mammoth.toml` file.
//! - [`api`]: what the inbound adapters share about their external contract, such as the error
//!   catalogue.
//! - [`http`]: inbound adapters for the Gradle protocol, health checks and the admin API.
//! - [`reapi`]: the inbound gRPC adapter for the REAPI cache (Bazel, Buck2, Pants, sbt 2, Please).
//! - [`server`]: connections, timeouts and the shutdown drain.
//! - [`logging`]: the JSON log line format and its buffered writer.
//! - [`telemetry`]: the Prometheus recorder behind `/metrics`.
//! - [`tls`]: the optional built-in TLS certificate and its reload.
//!
//! The crate is a library with a thin `mammoth` binary, so a custom build can supply its own
//! [`store::BlobStore`] implementation without forking.

pub mod api;
pub mod auth;
pub mod cache;
pub mod config;
pub mod http;
pub mod key;
pub mod logging;
pub mod reapi;
pub mod server;
pub mod store;
mod tee;
pub mod telemetry;
pub mod tls;

pub use http::{router, router_with, router_with_auth};
