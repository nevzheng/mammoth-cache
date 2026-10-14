//! Compiles the vendored REAPI and ByteStream protos with protox (no `protoc` needed) and writes
//! tonic servers and clients, one file per proto package, into the server crate.

use std::{error::Error, path::Path, process::Command};

fn main() -> Result<(), Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let protos = root.join("third_party/proto");
    let out = root.join("crates/mammoth-cache/src/reapi/proto");
    let files = protox::compile(
        [
            "build/bazel/remote/execution/v2/remote_execution.proto",
            "google/bytestream/bytestream.proto",
        ],
        [&protos],
    )?;
    std::fs::create_dir_all(&out)?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .build_transport(false)
        // Blob data as `Bytes`, so it moves to and from the tiers without copies.
        .bytes(".")
        // `google.rpc.Status` and its details come from tonic-types, whose helpers build and
        // read them.
        .extern_path(".google.rpc", "::tonic_types::pb")
        .emit_rerun_if_changed(false)
        .out_dir(&out)
        .compile_fds(files)?;
    // The protos are the documentation. Their comments are not Rust doc comments: they hold code
    // blocks that rustdoc would try to compile, and links it cannot resolve.
    for entry in std::fs::read_dir(&out)? {
        let path = entry?.path();
        let code = std::fs::read_to_string(&path)?;
        let stripped: String = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("///"))
            .flat_map(|line| [line, "\n"])
            .collect();
        std::fs::write(&path, stripped)?;
        // Bazel's rustfmt check formats every source file, `include!`d ones too.
        let formatted = Command::new("rustfmt")
            .arg("--config-path")
            .arg(root.join("rustfmt.toml"))
            .arg(&path)
            .status()?;
        if !formatted.success() {
            return Err(format!("rustfmt failed on {}", path.display()).into());
        }
    }
    Ok(())
}
