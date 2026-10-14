"""Shutdown as Kubernetes drives it (doc 0013): SIGTERM drains instead of dropping requests.

The Compose nodes keep the `[server]` defaults: `shutdown_delay` 5 s, `shutdown_timeout` 25 s.
"""

from __future__ import annotations

import time
from concurrent.futures import ThreadPoolExecutor
from typing import TYPE_CHECKING

import httpx
import pytest

from mammoth_e2e import blobs
from mammoth_e2e.client import CacheClient

if TYPE_CHECKING:
    from collections.abc import Iterator

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

SHUTDOWN_DELAY = 5.0
SHUTDOWN_TIMEOUT = 25.0
CHUNKS = 20
CHUNK_PAUSE = 0.5  # the upload takes about 10 s, past the end of shutdown_delay


def _slowly(body: bytes) -> Iterator[bytes]:
    step = len(body) // CHUNKS
    for i in range(CHUNKS):
        time.sleep(CHUNK_PAUSE)
        yield body[i * step : (i + 1) * step if i < CHUNKS - 1 else len(body)]


def _wait_for(url: str, status: int, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while httpx.get(url, timeout=2.0).status_code != status:
        assert time.monotonic() < deadline, f"{url} never answered {status}"
        time.sleep(0.05)


@pytest.mark.requires("disk")
@pytest.mark.control
def test_sigterm_finishes_a_store_in_flight(
    target: Target, client: CacheClient, controlled: Stack
) -> None:
    stored_key, stored = blobs.gradle_key(), blobs.payload(1000)
    assert client.gradle_put(stored_key, stored).status_code == 201
    key, body = blobs.gradle_key(), blobs.payload(2 * 1024 * 1024)
    uploader = CacheClient(target, credentials=target.credentials)
    try:
        with ThreadPoolExecutor(max_workers=1) as pool:
            # A fixed Content-Length, as Gradle sends, streamed slowly so it is still in flight.
            upload = pool.submit(uploader.put, f"/cache/{key}", _slowly(body),
                                 {"Content-Length": str(len(body))})  # fmt: skip
            time.sleep(1.0)
            controlled.kill("mammoth", "SIGTERM")
            signalled = time.monotonic()

            # During shutdown_delay the node leaves rotation but still serves.
            _wait_for(target.url("/readyz"), 503, timeout=2.0)
            assert httpx.get(target.url("/livez"), timeout=2.0).status_code == 200
            fresh = CacheClient(target, credentials=target.credentials)  # a new connection
            assert fresh.gradle_get(stored_key).content == stored
            fresh.close()
            assert time.monotonic() - signalled < SHUTDOWN_DELAY

            # The upload outlives shutdown_delay and still finishes.
            response = upload.result(timeout=SHUTDOWN_TIMEOUT)
            assert response.status_code == 201, response.text
            assert time.monotonic() - signalled > SHUTDOWN_DELAY

        assert controlled.wait_exited("mammoth", timeout=SHUTDOWN_TIMEOUT) == 0
        assert time.monotonic() - signalled < SHUTDOWN_TIMEOUT
    finally:
        uploader.close()
        controlled.start("mammoth")
    assert client.gradle_get(key).content == body
