"""Real Buck2 builds, local execution with the remote cache, against mammoth's gRPC listener.

Buck2 has no HTTP cache client, so REAPI is its only way to use mammoth (doc 0016). The fixture
brings its own rules instead of the prelude, so a build downloads nothing but Buck2 itself.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest
from pydantic import SecretStr

from mammoth_e2e.builds import Buck2Project
from mammoth_e2e.metrics import delta, scrape

if TYPE_CHECKING:
    from pathlib import Path

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

pytestmark = pytest.mark.requires("reapi")

ACTIONS = 2  # //:lib and //:app
BATCH_LIMIT = 4 * 1024 * 1024 - 256 * 1024


def _address(address: str | None) -> str:
    if address is None:
        pytest.skip("target has no REAPI address")
    return f"grpc://{address}"


@pytest.fixture
def project(tmp_path: Path, buck2_version: str, target: Target, live: Stack | None) -> Buck2Project:
    del live
    project = Buck2Project.create(tmp_path, buck2_version, _address(target.reapi_address))
    project.credentials = target.credentials
    return project


def test_store(project: Buck2Project, target: Target) -> None:
    before = scrape(target)
    result = project.build()
    assert result.ok, result.describe()
    assert result.executed() == ACTIONS, result.describe()
    after = scrape(target)
    grpc = "mammoth_grpc_requests_total"
    assert delta(before, after, grpc, method="UpdateActionResult", code="OK") >= ACTIONS
    assert delta(before, after, "mammoth_cache_stores_total", result="stored") >= 2 * ACTIONS


def test_hit(project: Buck2Project) -> None:
    assert project.build().ok
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()
    assert result.executed() == 0, result.describe()


def test_miss(project: Buck2Project) -> None:
    assert project.build().ok
    project.touch_app()
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == 1, result.describe()
    assert result.executed() == 1, result.describe()


def test_bytestream(project: Buck2Project, target: Target) -> None:
    """A blob over the batch limit goes up and comes back over ByteStream."""
    big = project.add_blob("big", BATCH_LIMIT + 1024 * 1024)
    before = scrape(target)
    assert project.build(big).ok
    middle = scrape(target)
    result = project.build(big)
    assert result.ok, result.describe()
    assert result.remote_hits() == 1, result.describe()
    grpc = "mammoth_grpc_requests_total"
    assert delta(before, middle, grpc, method="Write", code="OK") >= 1
    assert delta(middle, scrape(target), grpc, method="Read", code="OK") >= 1


@pytest.mark.requires("auth")
def test_read_only_user(project: Buck2Project, target: Target) -> None:
    assert project.build().ok
    project.credentials = target.user("developer")
    project.upload = False
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.requires("auth")
def test_token_not_logged(project: Buck2Project) -> None:
    result = project.build()
    assert project.credentials is not None
    assert project.credentials.token.get_secret_value() not in result.output


@pytest.mark.requires("auth")
def test_wrong_credentials(project: Buck2Project, target: Target) -> None:
    project.credentials = target.user("ci").model_copy(update={"token": SecretStr("mmc_wrong")})
    result = project.build()
    assert result.remote_hits() == 0, result.describe()
    assert "mmc_wrong" not in result.output


@pytest.mark.requires("auth")
def test_anonymous_read_only(tmp_path: Path, buck2_version: str, target: Target) -> None:
    project = Buck2Project.create(tmp_path, buck2_version, _address(target.open_reapi_address))
    project.credentials = target.user("ci")
    assert project.build().ok
    project.credentials = None
    project.upload = False
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()
