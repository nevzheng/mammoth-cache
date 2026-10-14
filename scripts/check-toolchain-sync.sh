#!/usr/bin/env bash
# Fails when the Rust version in MODULE.bazel differs from rust-toolchain.toml, so the Bazel and
# Cargo builds always use the same compiler.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo_version=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
bazel_version=$(sed -n 's/^    versions = \["\(.*\)"\],/\1/p' MODULE.bazel)
rustfmt_version=$(sed -n 's/^    rustfmt_version = "\(.*\)",/\1/p' MODULE.bazel)

if [[ "$cargo_version" != "$bazel_version" || "$cargo_version" != "$rustfmt_version" ]]; then
  echo "rust-toolchain.toml has $cargo_version but MODULE.bazel has versions=$bazel_version," \
    "rustfmt_version=$rustfmt_version; update MODULE.bazel to match." >&2
  exit 1
fi
echo "Rust $cargo_version in both rust-toolchain.toml and MODULE.bazel."
