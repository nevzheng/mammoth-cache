"""Real Bazel builds with `--remote_cache` against mammoth's HTTP cache (doc 0012)."""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest
from pydantic import SecretStr

from mammoth_e2e.builds import BazelProject
from mammoth_e2e.metrics import delta, scrape

if TYPE_CHECKING:
    from pathlib import Path

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

pytestmark = pytest.mark.requires("bazel")

ACTIONS = 2  # //lib and //app, one genrule each


@pytest.fixture
def project(tmp_path: Path, bazel_version: str, target: Target, live: Stack | None) -> BazelProject:
    del live
    project = BazelProject.create(tmp_path, bazel_version, target.url("/bazel"))
    project.credentials = target.credentials
    return project


def test_store(project: BazelProject, target: Target) -> None:
    before = scrape(target)
    result = project.build()
    assert result.ok, result.describe()
    after = scrape(target)
    puts = {"method": "PUT", "code": "201"}
    assert (
        delta(before, after, "mammoth_http_requests_total", route="/bazel/ac/{digest}", **puts)
        >= ACTIONS
    )
    assert (
        delta(before, after, "mammoth_http_requests_total", route="/bazel/cas/{digest}", **puts)
        >= ACTIONS
    )


def test_hit(project: BazelProject) -> None:
    assert project.build().ok
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


def test_miss(project: BazelProject) -> None:
    assert project.build().ok
    project.touch_app()
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == 1, result.describe()
    assert result.executed() == 1, result.describe()


@pytest.mark.requires("auth")
def test_bearer_header(project: BazelProject) -> None:
    project.auth_in_header = True
    assert project.build().ok
    result = project.build()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.requires("auth")
def test_token_not_logged(project: BazelProject) -> None:
    """Bazel prints the cache URL in some messages; the URL carries the token."""
    result = project.build()
    assert project.credentials is not None
    assert project.credentials.token.get_secret_value() not in result.output


@pytest.mark.requires("auth")
def test_wrong_credentials(project: BazelProject, target: Target) -> None:
    project.credentials = target.user("ci").model_copy(update={"token": SecretStr("mmc_wrong")})
    before = scrape(target)
    result = project.build()
    assert delta(before, scrape(target), "mammoth_http_requests_total", code="401") >= 1
    assert result.remote_hits() == 0, result.describe()


@pytest.mark.requires("auth")
def test_read_only_user(project: BazelProject, target: Target) -> None:
    assert project.build().ok
    project.credentials = target.user("developer")
    project.upload = False
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.requires("auth")
def test_anonymous_read_only(
    tmp_path: Path, bazel_version: str, target: Target, open_base_url: str
) -> None:
    project = BazelProject.create(tmp_path, bazel_version, target.url("/bazel", base=open_base_url))
    project.credentials = target.user("ci")
    assert project.build().ok
    project.credentials = None
    project.upload = False
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.control
def test_server_down(project: BazelProject, controlled: Stack) -> None:
    controlled.stop("mammoth")
    try:
        result = project.build()
        assert result.ok, "builds must pass without the cache\n" + result.describe()
    finally:
        controlled.start("mammoth")
