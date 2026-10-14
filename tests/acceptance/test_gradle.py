"""Real Gradle builds against mammoth, per doc 0008's scenario table."""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e.builds import GradleProject
from mammoth_e2e.metrics import delta, scrape

if TYPE_CHECKING:
    from pathlib import Path

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

# The fixture's cacheable tasks: compileJava in both modules.
CACHEABLE = {":lib:compileJava", ":app:compileJava"}


@pytest.fixture
def project(
    tmp_path: Path, gradle_version: str, target: Target, live: Stack | None
) -> GradleProject:
    del live
    return GradleProject.create(tmp_path, gradle_version, target.url("/cache/"))


def test_store(project: GradleProject, target: Target) -> None:
    before = scrape(target)
    result = project.build("build")
    assert result.ok, result.describe()
    assert result.executed() >= CACHEABLE, result.describe()
    stored = delta(
        before,
        scrape(target),
        "mammoth_http_requests_total",
        route="/cache/{key}",
        method="PUT",
        client="gradle",
        code="201",
    )
    assert stored >= len(CACHEABLE)


def test_hit(project: GradleProject) -> None:
    assert project.build("build").ok
    result = project.build("clean", "build")
    assert result.ok, result.describe()
    assert result.from_cache() >= CACHEABLE, result.describe()


def test_miss(project: GradleProject) -> None:
    assert project.build("build").ok
    project.touch_app()
    result = project.build("clean", "build")
    assert result.ok, result.describe()
    assert ":app:compileJava" in result.executed(), result.describe()
    assert ":lib:compileJava" in result.from_cache(), result.describe()


def test_expect_continue(project: GradleProject) -> None:
    project.expect_continue = True
    assert project.build("build").ok
    result = project.build("clean", "build")
    assert result.ok, result.describe()
    assert result.from_cache() >= CACHEABLE, result.describe()


@pytest.mark.requires("auth")
def test_basic_auth(project: GradleProject, target: Target) -> None:
    project.credentials = target.users["ci"]
    assert project.build("build").ok
    result = project.build("clean", "build")
    assert result.from_cache() >= CACHEABLE, result.describe()


@pytest.mark.requires("auth")
def test_wrong_credentials(project: GradleProject, target: Target) -> None:
    good = target.users["ci"]
    project.credentials = good.model_copy(update={"password": "wrong"})
    before = scrape(target)
    result = project.build("build")
    assert result.ok, "a cache error must not fail the build\n" + result.describe()
    assert "401" in result.output, result.describe()
    assert delta(before, scrape(target), "mammoth_http_requests_total", code="401") >= 1


@pytest.mark.requires("auth", "config")
def test_anonymous_read_only(project: GradleProject, target: Target) -> None:
    project.credentials = target.users["ci"]
    assert project.build("build").ok
    project.credentials = None
    result = project.build("clean", "build")
    assert result.ok, result.describe()
    assert result.from_cache() >= CACHEABLE, result.describe()


@pytest.mark.requires("size-limit")
def test_oversized_entry(project: GradleProject) -> None:
    """Needs a target whose entry limit is below the fixture's outputs."""
    result = project.build("build")
    assert result.ok, result.describe()
    assert "remote build cache" not in result.output.lower() or "disabled" not in result.output


@pytest.mark.control
def test_server_down(project: GradleProject, controlled: Stack) -> None:
    controlled.stop("mammoth")
    try:
        result = project.build("build")
        assert result.ok, "builds must pass without the cache\n" + result.describe()
    finally:
        controlled.start("mammoth")
