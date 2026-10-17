"""The Gradle protocol on `/cache/{key}`, end to end through the image (docs 0001, 0007)."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from typing import TYPE_CHECKING
from urllib.parse import quote

import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

from mammoth_e2e import blobs, raw
from mammoth_e2e.client import CacheClient, auth_headers
from mammoth_e2e.metrics import delta, scrape
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from mammoth_e2e.metrics import Scrape
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

MIB = 1024 * 1024


@pytest.mark.parametrize("size", [1, 64 * 1024, MIB], ids=["1B", "64KiB", "1MiB"])
def test_round_trip(client: CacheClient, size: int) -> None:
    key, body = blobs.gradle_key(), blobs.payload(size)
    assert client.gradle_get(key).status_code == 404
    assert client.gradle_put(key, body).status_code == 201
    loaded = client.gradle_get(key)
    assert loaded.status_code == 200
    assert loaded.headers["content-type"] == "application/octet-stream"
    assert loaded.content == body
    assert client.head(f"/cache/{key}").status_code == 200


def test_mid_size_entry_is_kept(client: CacheClient, target: Target) -> None:
    """An entry under the entry limit but over the memory tier's per-entry cap is kept below memory.

    A memory-only node has nowhere to keep it, so it is oversized there: accepted and dropped, as
    test_topologies.py::test_entry_larger_than_memory checks on the memory node.
    """
    size = (target.memory_max_entry_size + target.max_entry_size) // 2
    key, body = blobs.gradle_key(), blobs.payload(size)
    stored = client.gradle_put(key, body)
    if target.has(Feature.DISK) or target.has(Feature.BUCKET):
        assert stored.status_code == 201
        assert client.gradle_get(key).content == body
    else:
        assert stored.status_code == 200
        assert client.gradle_get(key).status_code == 404


def test_write_once(client: CacheClient) -> None:
    key = blobs.gradle_key()
    first, second = blobs.payload(100), blobs.payload(100)
    assert client.gradle_put(key, first).status_code == 201
    assert client.gradle_put(key, second).status_code == 200
    assert client.gradle_get(key).content == first


def test_oversize_is_accepted_and_dropped(
    client: CacheClient, target: Target, metrics_before: Scrape
) -> None:
    """Any non-2xx would turn Gradle's remote cache off, so oversize stores get 200 (doc 0007)."""
    key = blobs.gradle_key()
    response = client.gradle_put(key, blobs.payload(target.max_entry_size + 1))
    assert response.status_code == 200
    assert client.gradle_get(key).status_code == 404
    after = scrape(target)
    assert (
        delta(metrics_before, after, "mammoth_http_requests_total", method="PUT", code="200") >= 1
    )


def test_requests_are_counted_by_client(
    client: CacheClient, target: Target, metrics_before: Scrape
) -> None:
    key = blobs.gradle_key()
    client.gradle_put(key, b"x")
    client.gradle_get(key)
    after = scrape(target)
    labels = {"route": "/cache/{key}", "client": "other"}
    assert delta(metrics_before, after, "mammoth_http_requests_total", method="PUT", **labels) == 1
    assert delta(metrics_before, after, "mammoth_http_requests_total", method="GET", **labels) == 1


@settings(
    max_examples=50, deadline=None, suppress_health_check=[HealthCheck.function_scoped_fixture]
)
@given(key=st.text(min_size=1, max_size=80).filter(lambda k: k not in {".", ".."}))
def test_invalid_keys_are_rejected_cleanly(client: CacheClient, key: str) -> None:
    """Any key that is not lowercase hex is a 400, never a 5xx or a hit."""
    response = client.gradle_get(quote(key, safe=""))
    if key.isascii() and all(c in "0123456789abcdef" for c in key) and len(key) <= 128:
        assert response.status_code in {200, 404}
    else:
        assert response.status_code in {400, 404}, response.text


# Edge cases


@pytest.mark.control
def test_concurrent_stores_of_one_key(
    client: CacheClient, target: Target, controlled: Stack
) -> None:
    """Racing builds store one key: one write wins everywhere, and still wins after a restart."""
    keys = [blobs.gradle_key() for _ in range(5)]  # several rounds, so the race shows reliably
    winners: dict[str, bytes] = {}

    def store(key: str, body: bytes) -> int:
        c = CacheClient(target, credentials=target.credentials)
        try:
            return c.gradle_put(key, body).status_code
        finally:
            c.close()

    for key in keys:
        bodies = [blobs.payload(256 * 1024) for _ in range(16)]
        with ThreadPoolExecutor(max_workers=len(bodies)) as pool:
            statuses = list(pool.map(store, [key] * len(bodies), bodies))
        assert set(statuses) <= {200, 201}, statuses
        assert statuses.count(201) == 1, statuses
        winners[key] = bodies[statuses.index(201)]
        assert client.gradle_get(key).content == winners[key]
    controlled.restart("mammoth")
    for key, winner in winners.items():
        assert client.gradle_get(key).content == winner, "a restart served a different body"


def test_chunked_upload_without_length(client: CacheClient) -> None:
    key = blobs.gradle_key()
    chunks = [blobs.payload(64 * 1024) for _ in range(16)]
    assert client.gradle_put(key, iter(chunks)).status_code == 201
    assert client.gradle_get(key).content == b"".join(chunks)


def test_truncated_upload_stores_nothing(client: CacheClient, target: Target) -> None:
    """A build killed mid-upload must not leave a partial entry that later builds load."""
    key = blobs.gradle_key()
    headers = auth_headers(target.credentials)
    raw.truncated_put(str(target.base_url), f"/cache/{key}", headers, 100_000, b"x" * 50_000)
    assert client.gradle_get(key).status_code == 404


def test_head_on_a_miss(client: CacheClient) -> None:
    assert client.head(f"/cache/{blobs.gradle_key()}").status_code == 404


@pytest.mark.parametrize(
    "key",
    ["ABCDEF0123456789ABCDEF0123456789", "a" * 129, "zz", "%00", "0123-4567"],
    ids=["uppercase", "too-long", "not-hex", "nul", "dash"],
)
def test_bad_keys_are_400(client: CacheClient, key: str) -> None:
    assert client.gradle_put(key, b"x").status_code == 400
    assert client.gradle_get(key).status_code == 400


@pytest.mark.requires("disk")
def test_entry_at_the_limit_is_kept(client: CacheClient, target: Target) -> None:
    key, body = blobs.gradle_key(), blobs.payload(target.max_entry_size)
    assert client.gradle_put(key, body).status_code == 201
    assert client.gradle_get(key).content == body


def test_duplicate_store_reads_the_body_first(client: CacheClient, target: Target) -> None:
    """Re-storing a present key answers 200, but only after reading the body.

    CI agents often store a key another agent just stored. Answering early and closing the
    connection leaves the client writing into a closed socket: a broken pipe, which Gradle treats
    as a failed store and turns the remote cache off for the rest of the build.
    """
    key, body = blobs.gradle_key(), blobs.payload(MIB)
    assert client.gradle_put(key, body).status_code == 201
    headers = auth_headers(target.credentials)
    assert not raw.answers_before_body(str(target.base_url), f"/cache/{key}", headers, body)
