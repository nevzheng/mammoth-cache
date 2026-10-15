"""Contract edge cases found by reading the code against the OpenAPI files, pinned as fixed tests.

The model test explores at random; these are the cases worth running every time. A case that finds
a bug lands with the fix, or with the behaviour specified as it is; never as an xfail. A protocol
that is off answering like an unknown route is pinned in crates/mammoth-cache/tests/routes.rs, since
every Compose node has Bazel on. Duplicate-store draining and racing writers are covered in
test_gradle.py and test_bazel.py.
"""

from __future__ import annotations

import hashlib
import socket
import time
import uuid
from typing import TYPE_CHECKING
from urllib.parse import urlsplit

import httpx
import pytest

from mammoth_e2e import blobs

if TYPE_CHECKING:
    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target


@pytest.mark.parametrize("method", ["POST", "DELETE", "PATCH"])
def test_wrong_method_on_gradle_is_an_aip_193_error(writer: CacheClient, method: str) -> None:
    response = writer.request(method, f"/cache/{blobs.gradle_key()}")
    assert response.status_code == 405
    assert response.headers.get("content-type", "").startswith("application/json")
    assert response.json()["error"]["code"] == 405


@pytest.mark.requires("auth")
def test_maven_options_needs_no_credentials(target: Target, live: Stack | None) -> None:
    del live
    if "cache:read" in target.anonymous_scopes:
        pytest.skip("anonymous can read on this target, so the case does not arise")
    response = httpx.request("OPTIONS", target.url(f"/maven/{blobs.maven_path()}"))
    assert response.status_code == 204


@pytest.mark.requires("bazel")
def test_mismatched_empty_blob_is_refused(writer: CacheClient, contracts: Contracts) -> None:
    """A zero-length upload under a digest that isn't SHA-256 of nothing must not be stored."""
    digest = hashlib.sha256(uuid.uuid4().bytes).hexdigest()
    response = writer.put(f"/bazel/cas/{digest}", b"")
    assert response.status_code == 400
    contracts.check("/bazel/cas/{digest}", response)
    assert writer.get(f"/bazel/cas/{digest}").status_code == 404


@pytest.mark.requires("auth")
def test_refused_upload_keeps_the_connection_usable(target: Target, live: Stack | None) -> None:
    """After refusing a `PUT`, the node must either read the body or say it is closing.

    Clients send the body after the headers. If the node answers the headers and drops the
    connection, a keep-alive client's next request on it fails with no response.
    """
    del live
    if "cache:write" in target.anonymous_scopes:
        pytest.skip("anonymous can write on this target, so nothing is refused")
    url = urlsplit(target.url("/"))
    if url.scheme != "http" or url.hostname is None:
        pytest.skip("needs a plain HTTP target")
    key = blobs.gradle_key()
    body = b"refused"
    host = f"Host: {url.netloc}\r\n"
    put = f"PUT /cache/{key} HTTP/1.1\r\n{host}Content-Length: {len(body)}\r\n\r\n"
    get = f"GET /cache/{key} HTTP/1.1\r\n{host}\r\n"
    with socket.create_connection((url.hostname, url.port or 80), timeout=5) as conn:
        try:
            conn.sendall(put.encode())
            time.sleep(0.2)  # the body arrives after the node has seen the headers
            conn.sendall(body)
            head = conn.recv(65536).split(b"\r\n\r\n", 1)[0].decode().lower()
            assert head.startswith("http/1.1 401"), head
            if "\r\nconnection: close" in head:
                return
            conn.sendall(get.encode())
            time.sleep(0.2)
            answer = conn.recv(65536)
        except OSError as e:
            pytest.fail(f"the node dropped the connection: {e!r}")
    assert answer.startswith(b"HTTP/1.1 401"), "the node closed the connection silently"
