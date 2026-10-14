"""Real Gradle builds against mammoth, per doc 0008's scenario table."""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest
from pydantic import SecretStr

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
    project = GradleProject.create(tmp_path, gradle_version, target.url("/cache/"))
    project.credentials = target.credentials
    return project


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
def test_read_only_user_loads_but_does_not_store(project: GradleProject, target: Target) -> None:
    assert project.build("build").ok
    project.credentials = target.user("developer")
    project.push = False
    result = project.build("clean", "build")
    assert result.ok, result.describe()
    assert result.from_cache() >= CACHEABLE, result.describe()


@pytest.mark.requires("auth")
def test_wrong_credentials(project: GradleProject, target: Target) -> None:
    ci = target.user("ci")
    project.credentials = ci.model_copy(update={"token": SecretStr("mmc_wrong")})
    before = scrape(target)
    result = project.build("build")
    assert delta(before, scrape(target), "mammoth_http_requests_total", code="401") >= 1
    # Gradle treats a 401 as a broken cache: it reports it and the build either fails or turns
    # the remote cache off. Either way the user hears about it and nothing is stored.
    assert "401" in result.output or not result.ok, result.describe()
    assert not result.from_cache(), result.describe()


@pytest.mark.requires("auth")
def test_anonymous_read_only(
    tmp_path: Path, gradle_version: str, target: Target, open_base_url: str
) -> None:
    """Anonymous `cache:read`: loads hit; Gradle with push off sends no stores."""
    project = GradleProject.create(
        tmp_path, gradle_version, target.url("/cache/", base=open_base_url)
    )
    project.credentials = target.user("ci")
    assert project.build("build").ok
    project.credentials = None
    project.push = False
    result = project.build("clean", "build")
    assert result.ok, result.describe()
    assert result.from_cache() >= CACHEABLE, result.describe()


@pytest.mark.control
def test_server_down(project: GradleProject, controlled: Stack) -> None:
    controlled.stop("mammoth")
    try:
        result = project.build("build")
        assert result.ok, "builds must pass without the cache\n" + result.describe()
    finally:
        controlled.start("mammoth")
