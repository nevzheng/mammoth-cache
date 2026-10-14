"""Downloads and caches client distributions (Gradle, Maven) for the acceptance suite."""

from __future__ import annotations

import os
import platform
import shutil
import tempfile
import zipfile
from pathlib import Path

from mammoth_e2e import net
from mammoth_e2e.versions import MAVEN_CENTRAL, MAVEN_DIST


def cache_dir() -> Path:
    """`$MAMMOTH_E2E_CACHE`, else `$XDG_CACHE_HOME/mammoth-e2e`, else `~/.cache/mammoth-e2e`."""
    if explicit := os.environ.get("MAMMOTH_E2E_CACHE"):
        path = Path(explicit)
    else:
        xdg = os.environ.get("XDG_CACHE_HOME")
        path = (Path(xdg) if xdg else Path.home() / ".cache") / "mammoth-e2e"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _install(url: str, name: str) -> Path:
    """Download a zip distribution once and unpack it under the cache directory."""
    dest = cache_dir() / "dists" / name
    if dest.is_dir():
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=dest.parent) as tmp:
        archive = Path(tmp) / "dist.zip"
        with net.stream(url) as response, archive.open("wb") as f:
            for chunk in response.iter_bytes():
                f.write(chunk)
        with zipfile.ZipFile(archive) as z:
            z.extractall(tmp)
        (top,) = [p for p in Path(tmp).iterdir() if p.is_dir()]
        for script in (top / "bin").iterdir():
            script.chmod(0o755)
        # Rename is atomic, so a parallel worker never sees a half-unpacked distribution.
        try:
            top.rename(dest)
        except OSError:
            if not dest.is_dir():
                raise
            shutil.rmtree(top)
    return dest


def gradle_home(version: str) -> Path:
    url = f"https://services.gradle.org/distributions/gradle-{version}-bin.zip"
    return _install(url, f"gradle-{version}")


def maven_home(version: str) -> Path:
    url = f"{MAVEN_CENTRAL}/{MAVEN_DIST}/{version}/apache-maven-{version}-bin.zip"
    return _install(url, f"apache-maven-{version}")


def bazel_binary(version: str) -> Path:
    """The Bazel release binary for this platform, downloaded once from GitHub releases."""
    machine = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "arm64", "arm64": "arm64"}[
        platform.machine().lower()
    ]
    system = platform.system().lower()
    dest = cache_dir() / "dists" / f"bazel-{version}-{system}-{machine}"
    if dest.is_file():
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    url = (
        "https://github.com/bazelbuild/bazel/releases/download/"
        f"{version}/bazel-{version}-{system}-{machine}"
    )
    tmp = dest.with_suffix(f".{os.getpid()}.part")
    with net.stream(url) as response, tmp.open("wb") as f:
        for chunk in response.iter_bytes():
            f.write(chunk)
    tmp.chmod(0o755)
    tmp.rename(dest)
    return dest


def bazel_startup_args() -> list[str]:
    """Extra Bazel startup options from `$MAMMOTH_E2E_BAZEL_STARTUP_ARGS` (space-separated).

    For machines whose proxy needs its CA in Bazel's JVM, e.g.
    `--host_jvm_args=-Djavax.net.ssl.trustStore=/etc/ssl/certs/java/cacerts`.
    """
    return os.environ.get("MAMMOTH_E2E_BAZEL_STARTUP_ARGS", "").split()


def bazel_registries() -> list[str]:
    """Extra module registries from `$MAMMOTH_E2E_BAZEL_REGISTRIES` (comma-separated).

    For networks that block bcr.bazel.build; empty means Bazel's default registry.
    """
    raw = os.environ.get("MAMMOTH_E2E_BAZEL_REGISTRIES", "")
    return [r.strip() for r in raw.split(",") if r.strip()]
