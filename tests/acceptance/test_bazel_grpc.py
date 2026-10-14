"""Real Bazel builds with `--remote_cache=grpc://` against mammoth's REAPI listener (doc 0016).

The same scenarios as `test_bazel.py` over gRPC, plus what only gRPC has: blobs past the 4 MiB
batch limit (ByteStream), build without the bytes, and sharing entries with the HTTP cache.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest
from pydantic import SecretStr

from mammoth_e2e.builds import BazelProject
from mammoth_e2e.metrics import delta, scrape

if TYPE_CHECKING:
    from pathlib import Path

    from mammoth_e2e.builds import Download
    from mammoth_e2e.metrics import Scrape
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

pytestmark = pytest.mark.requires("reapi")

ACTIONS = 2  # //lib and //app, one genrule each
BATCH_LIMIT = 4 * 1024 * 1024  # max_batch_total_size_bytes; bigger blobs go over ByteStream


def _grpc_url(address: str | None) -> str:
    if address is None:
        pytest.skip("target has no REAPI address")
    return f"grpc://{address}"


@pytest.fixture
def project(tmp_path: Path, bazel_version: str, target: Target, live: Stack | None) -> BazelProject:
    del live
    project = BazelProject.create(tmp_path, bazel_version, _grpc_url(target.reapi_address))
    project.credentials = target.credentials
    return project


def _grpc(before: Scrape, after: Scrape, method: str, code: str = "OK") -> float:
    return delta(before, after, "mammoth_grpc_requests_total", method=method, code=code)


def test_store(project: BazelProject, target: Target) -> None:
    before = scrape(target)
    result = project.build()
    assert result.ok, result.describe()
    after = scrape(target)
    assert _grpc(before, after, "GetCapabilities") >= 1
    assert _grpc(before, after, "UpdateActionResult") >= ACTIONS
    # Bazel asks which outputs are missing, then uploads each over ByteStream, never in batches.
    assert _grpc(before, after, "FindMissingBlobs") >= 1
    assert _grpc(before, after, "Write") >= 2 * ACTIONS
    assert delta(before, after, "mammoth_cache_stores_total", result="stored") >= 2 * ACTIONS


def test_hit(project: BazelProject) -> None:
    first = project.build()
    assert first.ok, first.describe()
    outputs = project.outputs()
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()
    assert project.outputs() == outputs


def test_miss(project: BazelProject) -> None:
    assert project.build().ok
    project.touch_app()
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == 1, result.describe()
    assert result.executed() == 1, result.describe()


@pytest.mark.parametrize("download", ["minimal", "toplevel", "all"])
def test_download_modes(project: BazelProject, download: Download) -> None:
    """Build without the bytes trusts `GetActionResult` to only return results it can serve."""
    assert project.build().ok
    project.download = download
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


def test_bytestream(project: BazelProject, target: Target) -> None:
    """A blob over the batch limit goes up with ByteStream.Write and down with ByteStream.Read."""
    big = project.add_blob("big", BATCH_LIMIT + 1024 * 1024)
    before = scrape(target)
    assert project.build(big).ok
    outputs = project.outputs()
    after_store = scrape(target)
    assert _grpc(before, after_store, "Write") >= 1
    result = project.build(big)
    assert result.ok, result.describe()
    assert result.remote_hits() == 1, result.describe()
    assert _grpc(after_store, scrape(target), "Read") >= 1
    assert project.outputs() == outputs


def test_empty_output(project: BazelProject) -> None:
    """The empty blob is always present and never stored (doc 0016)."""
    empty = project.add_blob("empty", 0)
    assert project.build(empty).ok
    result = project.build(empty)
    assert result.ok, result.describe()
    assert result.remote_hits() == 1, result.describe()
    assert project.outputs() == {"gen/empty.out": b""}


@pytest.mark.requires("bazel")
@pytest.mark.parametrize("first", ["http", "grpc"])
def test_shared_with_http(project: BazelProject, target: Target, first: str) -> None:
    """Entries written over one protocol are hits over the other: same namespaces and keys."""
    grpc_url = project.cache_url
    http_url = target.url("/bazel")
    project.cache_url = http_url if first == "http" else grpc_url
    assert project.build().ok
    project.cache_url = grpc_url if first == "http" else http_url
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.requires("auth")
def test_token_not_logged(project: BazelProject) -> None:
    result = project.build()
    assert project.credentials is not None
    assert project.credentials.token.get_secret_value() not in result.output


@pytest.mark.requires("auth")
def test_wrong_credentials(project: BazelProject, target: Target) -> None:
    """Bazel asks for capabilities first, so a bad token fails the build with the node's reason.

    Over HTTP the same build passes uncached; over gRPC it stops, which is Bazel's choice.
    """
    project.credentials = target.user("ci").model_copy(update={"token": SecretStr("mmc_wrong")})
    result = project.build()
    assert not result.ok, result.describe()
    assert "UNAUTHENTICATED: The credentials are not valid." in result.output, result.describe()
    assert "mmc_wrong" not in result.output


@pytest.mark.requires("auth")
def test_read_only_user(project: BazelProject, target: Target) -> None:
    assert project.build().ok
    project.credentials = target.user("developer")
    project.upload = False
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.requires("auth")
def test_read_only_user_uploading(project: BazelProject, target: Target) -> None:
    """A read-only token's uploads are refused and stored nowhere; the build still passes."""
    project.credentials = target.user("developer")
    before = scrape(target)
    result = project.build()
    assert result.ok, result.describe()
    # Bazel 8 says "errors during bulk transfer", Bazel 9 quotes the PERMISSION_DENIED.
    assert "WARNING: Remote Cache:" in result.output, result.describe()
    assert delta(before, scrape(target), "mammoth_cache_stores_total") == 0
    project.credentials = target.user("ci")
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == 0, result.describe()


@pytest.mark.requires("auth")
def test_anonymous_read_only(tmp_path: Path, bazel_version: str, target: Target) -> None:
    project = BazelProject.create(tmp_path, bazel_version, _grpc_url(target.open_reapi_address))
    project.credentials = target.user("ci")
    assert project.build().ok
    project.credentials = None
    project.upload = False
    result = project.build()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()


@pytest.mark.control
def test_server_down(project: BazelProject, controlled: Stack) -> None:
    """Unlike HTTP, Bazel fails a build whose gRPC cache it cannot reach (docs/user/reapi.md)."""
    controlled.stop("mammoth")
    try:
        result = project.build()
    finally:
        controlled.start("mammoth")
    assert not result.ok, result.describe()
    assert "Failed to query remote execution capabilities" in result.output, result.describe()


@pytest.mark.control
@pytest.mark.requires("bucket")
def test_bucket_down(project: BazelProject, controlled: Stack) -> None:
    """With the bucket stopped, the node serves from its local tiers and never fails a call."""
    controlled.stop("seaweedfs")
    try:
        assert project.build().ok
        result = project.build()
    finally:
        controlled.start("seaweedfs")
        controlled.wait_bucket()
    assert result.ok, result.describe()
    assert result.remote_hits() == ACTIONS, result.describe()
