"""The Gradle protocol on `/cache/{key}`, end to end through the image (docs 0001, 0007)."""

from __future__ import annotations

from typing import TYPE_CHECKING
from urllib.parse import quote

import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

from mammoth_e2e import blobs
from mammoth_e2e.metrics import delta, scrape
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.metrics import Scrape
    from mammoth_e2e.target import Target

MIB = 1024 * 1024


@pytest.mark.parametrize("size", [1, 64 * 1024, 4 * MIB], ids=["1B", "64KiB", "4MiB"])
def test_round_trip(client: CacheClient, size: int) -> None:
    key, body = blobs.gradle_key(), blobs.payload(size)
    assert client.gradle_get(key).status_code == 404
    assert client.gradle_put(key, body).status_code == 201
    loaded = client.gradle_get(key)
    assert loaded.status_code == 200
    assert loaded.headers["content-type"] == "application/octet-stream"
    assert loaded.content == body
    assert client.head(f"/cache/{key}").status_code == 200


def test_mid_size_entry_is_kept(
    client: CacheClient, target: Target, request: pytest.FixtureRequest
) -> None:
    """An entry under the entry limit but over the memory tier's per-entry cap must be kept."""
    if not target.has(Feature.DISK):
        request.applymarker(
            pytest.mark.xfail(
                strict=True,
                reason="memory-only binary answers 201 but drops entries over 16 MiB (audit)",
            )
        )
    key, body = blobs.gradle_key(), blobs.payload(32 * MIB)
    assert client.gradle_put(key, body).status_code == 201
    assert client.gradle_get(key).content == body


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
