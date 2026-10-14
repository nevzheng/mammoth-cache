"""`disk.size = "max-available"` (#85): the disk tier takes the volume less `free_space_buffer`.

The `max-available` node's `disk.path` is a 64 MiB tmpfs with a 16 MiB buffer, so the tier
holds 48 MiB. Filling it past that evicts the oldest entries; every store still succeeds.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e import blobs, metrics
from mammoth_e2e.client import CacheClient

if TYPE_CHECKING:
    from collections.abc import Iterator

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

MIB = 1024 * 1024
VOLUME = 64 * MIB  # the tmpfs size in deploy/compose/compose.yaml
BUFFER = 16 * MIB  # disk.free_space_buffer in deploy/compose/config/max-available.toml
ENTRY = 4 * MIB


@pytest.fixture
def base(target: Target, live: Stack | None) -> str:
    del live
    if "max-available" not in target.topologies:
        pytest.skip(f"target {target.name!r} has no max-available node")
    return str(target.topologies["max-available"]).rstrip("/")


@pytest.fixture
def node(target: Target, base: str) -> Iterator[CacheClient]:
    client = CacheClient(target, base_url=base)
    yield client
    client.close()


def test_capacity_is_the_volume_less_the_buffer(target: Target, base: str) -> None:
    scrape = metrics.scrape(target, base=base)
    assert scrape.value("mammoth_tier_capacity_bytes", tier="disk") == VOLUME - BUFFER


def test_filling_past_capacity_evicts(target: Target, base: str, node: CacheClient) -> None:
    """Twice the capacity in entries: every store succeeds and the oldest are evicted."""
    keys = [blobs.gradle_key() for _ in range(2 * (VOLUME - BUFFER) // ENTRY)]
    for key in keys:
        assert node.gradle_put(key, blobs.payload(ENTRY, seed=key)).status_code == 201

    scrape = metrics.scrape(target, base=base)
    assert scrape.value("mammoth_tier_size_bytes", tier="disk") <= VOLUME - BUFFER
    assert node.gradle_get(keys[0]).status_code == 404
    assert node.gradle_get(keys[-1]).content == blobs.payload(ENTRY, seed=keys[-1])
