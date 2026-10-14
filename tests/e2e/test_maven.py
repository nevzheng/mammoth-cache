"""The Maven extension's routes on `/maven/{path}`, end to end through the image (doc 0001)."""

from __future__ import annotations

import uuid
from typing import TYPE_CHECKING

import pytest

from mammoth_e2e import blobs, raw
from mammoth_e2e.client import auth_headers

if TYPE_CHECKING:
    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.target import Target


def test_round_trip(client: CacheClient) -> None:
    path, body = blobs.maven_path(), blobs.payload(10_000)
    assert client.maven_get(path).status_code == 404
    assert client.maven_put(path, body).status_code == 201
    assert client.maven_get(path).content == body


def test_overwrite(client: CacheClient) -> None:
    path = blobs.maven_path()
    first, second = blobs.payload(100), blobs.payload(100)
    assert client.maven_put(path, first).status_code == 201
    assert client.maven_put(path, second).status_code == 201
    assert client.maven_get(path).content == second


def test_options_without_dav(client: CacheClient) -> None:
    response = client.request("OPTIONS", f"/maven/{blobs.maven_path()}")
    assert response.status_code == 204
    assert "PUT" in response.headers["allow"]
    assert "dav" not in {h.lower() for h in response.headers}


def test_mkcol_is_a_no_op(client: CacheClient) -> None:
    assert client.request("MKCOL", "/maven/v1.1/dev.mammoth.e2e").status_code in {201, 405}


def test_oversize_is_rejected(client: CacheClient, target: Target) -> None:
    """Maven only logs a failed store, so oversize stays a 413 (doc 0007)."""
    path = blobs.maven_path()
    assert client.maven_put(path, blobs.payload(target.max_entry_size + 1)).status_code == 413
    assert client.maven_get(path).status_code == 404


def test_traversal_is_rejected(client: CacheClient) -> None:
    response = client.get("/maven/v1.1/%2e%2e/%2e%2e/etc/passwd")
    assert response.status_code in {400, 404}


# Edge cases


@pytest.mark.parametrize("layout", ["v1.1", "v1.2"], ids=["ext-1.2", "ext-1.3"])
def test_extension_layouts(client: CacheClient, layout: str) -> None:
    """1.2.x writes under v1.1/, 1.3.x under v1.2/; both are plain paths to the server."""
    checksum = blobs.gradle_key()[:16]
    for name in ("buildinfo.xml", "lib.jar", "lib-extra.zip"):
        path = f"{layout}/dev.mammoth.e2e/lib/{checksum}/{name}"
        body = blobs.payload(512)
        assert client.maven_put(path, body).status_code == 201
        assert client.maven_get(path).content == body


def test_build_report_path(client: CacheClient) -> None:
    path = f"v1.1/dev.mammoth.e2e/fixture/{uuid.uuid4()}/build-cache-report.xml"
    assert client.maven_put(path, b"<report/>").status_code == 201
    assert client.maven_get(path).content == b"<report/>"


@pytest.mark.parametrize(
    "path",
    [
        "v1.1/../../etc/passwd",
        "v1.1/%2e%2e/%2e%2e/etc/passwd",
        "v1.1/./x",
        "v1.1//x",
        "v1.1/a%00b",
        "v1.1/" + "/".join(["d"] * 64),
        "v1.1/" + "a" * 2048,
    ],
    ids=["dotdot", "encoded-dotdot", "dot", "empty-segment", "nul", "too-deep", "too-long"],
)
@pytest.mark.usefixtures("live")
def test_bad_paths_are_rejected(target: Target, path: str) -> None:
    """Sent raw: httpx would remove dot segments before the server saw them."""
    headers = auth_headers(target.credentials)
    base = str(target.base_url)
    assert raw.status(base, "PUT", f"/maven/{path}", headers, b"x") in {400, 404}
    assert raw.status(base, "GET", f"/maven/{path}", headers) in {400, 404}


def test_chunked_upload_without_length(client: CacheClient) -> None:
    path = blobs.maven_path()
    chunks = [blobs.payload(32 * 1024) for _ in range(8)]
    assert client.put(f"/maven/{path}", iter(chunks)).status_code == 201
    assert client.maven_get(path).content == b"".join(chunks)


def test_unsupported_methods(client: CacheClient) -> None:
    for method in ("DELETE", "POST", "PROPFIND"):
        response = client.request(method, f"/maven/{blobs.maven_path()}")
        assert response.status_code == 405, (method, response.status_code)
        assert "allow" in response.headers
