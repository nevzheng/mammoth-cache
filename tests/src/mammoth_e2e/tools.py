"""Downloads and caches client distributions (Gradle, Maven) for the acceptance suite."""

from __future__ import annotations

import os
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
