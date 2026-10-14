"""Bazel's HTTP cache on `/bazel/ac/` and `/bazel/cas/` (doc 0012)."""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e import blobs, raw
from mammoth_e2e.client import auth_headers, sha256

if TYPE_CHECKING:
    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.target import Target

pytestmark = pytest.mark.requires("bazel")


def reason(text: str) -> str:
    import json  # noqa: PLC0415 - only for error bodies

    return str(json.loads(text)["error"]["details"][0]["reason"])


def test_cas_round_trip(client: CacheClient) -> None:
    body = blobs.payload(10_000)
    digest = sha256(body)
    assert client.bazel_get("cas", digest).status_code == 404
    assert client.bazel_put("cas", digest, body).status_code == 201
    assert client.bazel_get("cas", digest).content == body
    assert client.head(f"/bazel/cas/{digest}").status_code == 200


def test_cas_is_immutable(client: CacheClient) -> None:
    body = blobs.payload(100)
    digest = sha256(body)
    assert client.bazel_put("cas", digest, body).status_code == 201
    assert client.bazel_put("cas", digest, body).status_code == 200


def test_cas_digest_mismatch_stores_nothing(client: CacheClient) -> None:
    body = blobs.payload(100)
    digest = sha256(body + b"!")
    response = client.bazel_put("cas", digest, body)
    assert response.status_code == 400
    assert reason(response.text) == "DIGEST_MISMATCH"
    assert client.bazel_get("cas", digest).status_code == 404


def test_cas_mismatch_in_a_chunked_upload(client: CacheClient) -> None:
    """Without Content-Length the hash is checked at the end of the stream."""
    chunks = [blobs.payload(4096) for _ in range(8)]
    digest = sha256(b"".join(chunks) + b"tail")
    response = client.put(f"/bazel/cas/{digest}", iter(chunks))
    assert response.status_code == 400
    assert client.bazel_get("cas", digest).status_code == 404


def test_empty_blob(client: CacheClient) -> None:
    digest = sha256(b"")
    assert client.bazel_put("cas", digest, b"").status_code in {200, 201}
    assert client.bazel_get("cas", digest).content == b""


def test_ac_round_trip_and_overwrite(client: CacheClient) -> None:
    """AC keys are action digests, so a non-hermetic action may store a new result."""
    digest = sha256(blobs.payload(32))
    first, second = blobs.payload(200), blobs.payload(200)
    assert client.bazel_put("ac", digest, first).status_code == 201
    assert client.bazel_get("ac", digest).content == first
    assert client.bazel_put("ac", digest, second).status_code == 201
    assert client.bazel_get("ac", digest).content == second


@pytest.mark.parametrize(
    "digest",
    ["0" * 63, "0" * 65, "A" * 64, "g" * 64, "../" + "0" * 61],
    ids=["short", "long", "uppercase", "not-hex", "traversal"],
)
@pytest.mark.parametrize("store", ["ac", "cas"])
@pytest.mark.usefixtures("live")
def test_malformed_digest(target: Target, store: str, digest: str) -> None:
    headers = auth_headers(target.credentials)
    path = f"/bazel/{store}/{digest}"
    assert raw.status(str(target.base_url), "GET", path, headers) in {400, 404}
    assert raw.status(str(target.base_url), "PUT", path, headers, b"x") in {400, 404}


def test_unknown_store(client: CacheClient) -> None:
    assert client.bazel_get("blobs", "0" * 64).status_code == 404


def test_oversize_is_413(client: CacheClient, target: Target) -> None:
    """Bazel logs a failed upload and keeps the cache on, so oversize is an honest 413."""
    body = blobs.payload(target.max_entry_size + 1)
    response = client.bazel_put("ac", sha256(body), body)
    assert response.status_code == 413


def test_namespaces_do_not_collide(client: CacheClient) -> None:
    body = blobs.payload(64)
    digest = sha256(body)
    assert client.bazel_put("cas", digest, body).status_code == 201
    assert client.bazel_get("ac", digest).status_code == 404
    assert client.gradle_get(digest[:32]).status_code == 404


@pytest.mark.parametrize(
    "store",
    [
        pytest.param(
            "cas",
            marks=pytest.mark.xfail(
                strict=True, reason="a duplicate store is answered before its body is read"
            ),
        ),
        "ac",  # AC stores replace, so the body is always read
    ],
)
def test_duplicate_store_reads_the_body_first(
    client: CacheClient, target: Target, store: str
) -> None:
    """Bazel pools connections; an early answer that drops one breaks its next upload."""
    body = blobs.payload(1024 * 1024)
    digest = sha256(body)
    assert client.bazel_put(store, digest, body).status_code == 201
    headers = auth_headers(target.credentials)
    path = f"/bazel/{store}/{digest}"
    assert not raw.answers_before_body(str(target.base_url), path, headers, body)
