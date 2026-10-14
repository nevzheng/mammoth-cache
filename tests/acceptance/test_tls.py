"""Real Gradle, Maven and Bazel builds against a node serving HTTPS itself (doc 0014).

Each client trusts the test CA the way the deployment guide tells operators to trust a private
CA: Gradle and Maven through a JVM trust store with the CA imported, Bazel with
`--tls_certificate`. The node is `mammoth-tls` on the Compose stack.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e.builds import BazelProject, GradleProject, MavenProject, java_trust_store

if TYPE_CHECKING:
    from pathlib import Path

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target, Tls

pytestmark = pytest.mark.requires("tls")

GRADLE_CACHEABLE = {":lib:compileJava", ":app:compileJava"}
MAVEN_MODULES = ("lib", "app")


@pytest.fixture
def tls(target: Target, live: Stack | None) -> Tls:
    del live
    if target.tls is None:
        pytest.skip(f"target {target.name!r} has no TLS nodes")
    return target.tls


@pytest.fixture
def trust_store(tls: Tls, tmp_path: Path) -> Path:
    return java_trust_store(tls.cert_dir / "ca.crt", tmp_path / "truststore.p12")


def test_gradle(
    tmp_path: Path, gradle_version: str, target: Target, tls: Tls, trust_store: Path
) -> None:
    url = target.url("/cache/", base=tls.base_url)
    project = GradleProject.create(tmp_path, gradle_version, url)
    project.credentials = target.credentials
    project.trust_store = trust_store
    stored = project.build("build")
    assert stored.ok, stored.describe()
    hit = project.build("clean", "build")
    assert hit.ok, hit.describe()
    assert hit.from_cache() >= GRADLE_CACHEABLE, hit.describe()


def test_maven(
    tmp_path: Path, maven_versions: tuple[str, str], target: Target, tls: Tls, trust_store: Path
) -> None:
    maven, extension = maven_versions
    url = target.url("/maven", base=tls.base_url)
    project = MavenProject.create(tmp_path, maven, extension, url)
    project.credentials = target.credentials
    project.trust_store = trust_store
    stored = project.build()
    assert stored.ok, stored.describe()
    hit = project.build()
    assert hit.ok, hit.describe()
    # Every build has a fresh local cache, so a restore can only come from the node.
    for module in MAVEN_MODULES:
        restoring = f"Found cached build, restoring dev.mammoth.fixture:{module} from cache"
        assert restoring in hit.output, hit.describe()
    assert "Build info downloaded from remote" in hit.output, hit.describe()


@pytest.mark.requires("bazel")
def test_bazel(tmp_path: Path, bazel_version: str, target: Target, tls: Tls) -> None:
    url = target.url("/bazel", base=tls.base_url)
    project = BazelProject.create(tmp_path, bazel_version, url)
    project.credentials = target.credentials
    project.tls_certificate = tls.cert_dir / "ca.crt"
    stored = project.build()
    assert stored.ok, stored.describe()
    hit = project.build()
    assert hit.ok, hit.describe()
    assert hit.remote_hits() == 2, hit.describe()
    assert hit.executed() == 0, hit.describe()


@pytest.mark.requires("bazel")
@pytest.mark.requires("reapi")
def test_bazel_grpcs(tmp_path: Path, bazel_version: str, target: Target, tls: Tls) -> None:
    """The REAPI listener serves the same certificate as HTTP (docs/user/reapi.md)."""
    if tls.reapi_address is None:
        pytest.skip("target's TLS node has no REAPI address")
    project = BazelProject.create(tmp_path, bazel_version, f"grpcs://{tls.reapi_address}")
    project.credentials = target.credentials
    project.tls_certificate = tls.cert_dir / "ca.crt"
    stored = project.build()
    assert stored.ok, stored.describe()
    hit = project.build()
    assert hit.ok, hit.describe()
    assert hit.remote_hits() == 2, hit.describe()
    assert hit.executed() == 0, hit.describe()
