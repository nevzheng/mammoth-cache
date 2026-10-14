"""The Maven extension's routes on `/maven/{path}`, end to end through the image (doc 0001)."""

from __future__ import annotations

from typing import TYPE_CHECKING

from mammoth_e2e import blobs

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
