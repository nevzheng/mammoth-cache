"""A bucket outage (audit finding 4, fixed in #51): local tiers keep serving, and it shows.

Two kinds of outage, because clients see them differently: `stop` refuses connections at once,
`pause` accepts them and never answers, like a network blackhole. Behind both, the node gives each
bucket call a short deadline, then skips the bucket for a cooldown, so misses stay fast 404s and
stores stay 201s. `mammoth_tier_up{tier="bucket"}` drops while it is skipped and comes back after;
`/readyz` stays 200, because the node can still serve. The checks read `/metrics` directly and
through Prometheus, the way an operator's alert would.
"""

from __future__ import annotations

import time
from typing import TYPE_CHECKING

import httpx
import pytest

from mammoth_e2e import blobs, metrics

if TYPE_CHECKING:
    from collections.abc import Callable

    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.prom import Prometheus
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

FIRST_MISS_BUDGET = 3.0  # the bucket deadline (about 2 s) plus slack
MISS_BUDGET = 0.5  # during the cooldown the bucket is skipped outright
RECOVERY_BUDGET = 45.0  # the 30 s cooldown, then a probe, plus Prometheus' scrape interval

pytestmark = [pytest.mark.requires("bucket"), pytest.mark.control]


def _eventually(check: Callable[[], bool], timeout: float, what: str) -> None:
    deadline = time.monotonic() + timeout
    while not check():
        assert time.monotonic() < deadline, f"{what} within {timeout:.0f}s"
        time.sleep(0.5)


def _timed[T](call: Callable[[], T]) -> tuple[T, float]:
    started = time.monotonic()
    try:
        result = call()
    except httpx.TimeoutException:
        pytest.fail("a request timed out while the bucket was down")
    return result, time.monotonic() - started


def _bucket_up(target: Target) -> float:
    return metrics.scrape(target).value("mammoth_tier_up", tier="bucket")


def _bucket_up_in_prometheus(prometheus: Prometheus) -> float | None:
    found = prometheus.query('mammoth_tier_up{tier="bucket", instance="mammoth:5071"}')
    return found[0][1] if found else None


@pytest.mark.parametrize("outage", ["stop", "pause"])
def test_bucket_outage_keeps_serving_and_shows_in_metrics(
    client: CacheClient,
    target: Target,
    controlled: Stack,
    prometheus: Prometheus,
    outage: str,
) -> None:
    stored_key, stored = blobs.gradle_key(), blobs.payload(1000)
    assert client.gradle_put(stored_key, stored).status_code == 201
    _eventually(lambda: _bucket_up(target) == 1, 10.0, "the bucket is up before the outage")

    down, up = (controlled.stop, controlled.start)
    if outage == "pause":
        down, up = controlled.pause, controlled.unpause
    down("seaweedfs")
    try:
        miss, took = _timed(lambda: client.gradle_get(blobs.gradle_key()))
        assert miss.status_code == 404
        assert took < FIRST_MISS_BUDGET
        for _ in range(10):
            miss, took = _timed(lambda: client.gradle_get(blobs.gradle_key()))
            assert miss.status_code == 404
            assert took < MISS_BUDGET
        put, took = _timed(lambda: client.gradle_put(blobs.gradle_key(), blobs.payload(1000)))
        assert put.status_code == 201
        assert took < FIRST_MISS_BUDGET
        assert client.gradle_get(stored_key).content == stored
        assert httpx.get(target.url("/readyz"), timeout=2.0).status_code == 200

        _eventually(lambda: _bucket_up(target) == 0, 5.0, "/metrics shows the bucket down")
        _eventually(
            lambda: _bucket_up_in_prometheus(prometheus) == 0, 10.0, "Prometheus sees it down"
        )
    finally:
        up("seaweedfs")

    # The gauge follows the bucket's last operation, so keep asking: after the cooldown a miss
    # tries the bucket again and succeeds.
    def recovered() -> bool:
        client.gradle_get(blobs.gradle_key())
        return _bucket_up(target) == 1

    _eventually(recovered, RECOVERY_BUDGET, "the bucket comes back")
    _eventually(lambda: _bucket_up_in_prometheus(prometheus) == 1, 10.0, "Prometheus sees it back")
