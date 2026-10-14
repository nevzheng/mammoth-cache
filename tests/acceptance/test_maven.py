"""Real Maven builds with the Build Cache Extension against mammoth, per doc 0008."""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e.builds import BuildResult, MavenProject
from mammoth_e2e.metrics import delta, scrape

if TYPE_CHECKING:
    from pathlib import Path

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

MODULES = ("lib", "app")


def restored(result: BuildResult, module: str) -> bool:
    """The module's build info came from mammoth and the extension restored it.

    Every build uses a fresh local cache, so a restore can only come from the server.
    """
    lines = result.output.splitlines()
    downloaded = any(
        "Build info downloaded from remote" in line and f"/dev.mammoth.fixture/{module}/" in line
        for line in lines
    )
    restoring = f"Found cached build, restoring dev.mammoth.fixture:{module} from cache"
    return downloaded and any(restoring in line for line in lines)


@pytest.fixture
def project(
    tmp_path: Path, maven_versions: tuple[str, str], target: Target, live: Stack | None
) -> MavenProject:
    del live
    maven, extension = maven_versions
    return MavenProject.create(tmp_path, maven, extension, target.url("/maven"))


def test_store(project: MavenProject, target: Target) -> None:
    before = scrape(target)
    result = project.build()
    assert result.ok, result.describe()
    stored = delta(
        before,
        scrape(target),
        "mammoth_http_requests_total",
        route="/maven/{*path}",
        method="PUT",
        client="maven",
        code="201",
    )
    assert stored >= len(MODULES), result.describe()


def test_hit(project: MavenProject) -> None:
    assert project.build().ok
    result = project.build()
    assert result.ok, result.describe()
    assert all(restored(result, m) for m in MODULES), result.describe()


def test_miss(project: MavenProject) -> None:
    assert project.build().ok
    project.touch_app()
    result = project.build()
    assert result.ok, result.describe()
    assert restored(result, "lib"), result.describe()
    assert not restored(result, "app"), result.describe()


@pytest.mark.requires("auth")
def test_basic_auth(project: MavenProject, target: Target) -> None:
    project.credentials = target.users["ci"]
    assert project.build().ok
    result = project.build()
    assert all(restored(result, m) for m in MODULES), result.describe()


@pytest.mark.requires("auth")
def test_challenge(project: MavenProject, target: Target) -> None:
    """Maven sends credentials only after a 401 with WWW-Authenticate on GET."""
    project.credentials = target.users["ci"]
    before = scrape(target)
    assert project.build().ok
    result = project.build()
    assert all(restored(result, m) for m in MODULES), result.describe()
    assert delta(before, scrape(target), "mammoth_http_requests_total", code="401") >= 1


@pytest.mark.requires("auth")
def test_wrong_credentials(project: MavenProject, target: Target) -> None:
    project.credentials = target.users["ci"].model_copy(update={"password": "wrong"})
    result = project.build()
    assert result.ok, "a cache error must not fail the build\n" + result.describe()


@pytest.mark.requires("auth", "config")
def test_anonymous_read_only(project: MavenProject, target: Target) -> None:
    project.credentials = target.users["ci"]
    assert project.build().ok
    project.credentials = None
    result = project.build()
    assert result.ok, result.describe()
    assert all(restored(result, m) for m in MODULES), result.describe()


@pytest.mark.control
def test_server_down(project: MavenProject, controlled: Stack) -> None:
    controlled.stop("mammoth")
    try:
        result = project.build()
        assert result.ok, "builds must pass without the cache\n" + result.describe()
    finally:
        controlled.start("mammoth")
