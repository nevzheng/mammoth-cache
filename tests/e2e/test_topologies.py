"""The same checks on every tier topology the config allows (doc 0003): memory only, memory and
disk, memory and the bucket, and all three.

What changes between them is what survives a restart and how large an entry can be: without a
bucket, the largest entry is the deepest local tier's `max_entry_size`, so a memory-only node
treats anything over `memory.max_entry_size` as oversized.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

import pytest

from mammoth_e2e import blobs
from mammoth_e2e.client import CacheClient, sha256

if TYPE_CHECKING:
    from collections.abc import Iterator

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target


@dataclass(frozen=True, slots=True)
class Topology:
    name: str
    service: str  # the Compose service, for restarts
    disk: bool
    bucket: bool


TOPOLOGIES = [
    Topology("memory", "mammoth-memory", disk=False, bucket=False),
    Topology("memory-disk", "mammoth-memory-disk", disk=True, bucket=False),
    Topology("memory-bucket", "mammoth-memory-bucket", disk=False, bucket=True),
    Topology("full", "mammoth", disk=True, bucket=True),
]


@pytest.fixture(params=TOPOLOGIES, ids=lambda t: t.name)
def topology(request: pytest.FixtureRequest) -> Topology:
    return request.param


@pytest.fixture
def node(target: Target, live: Stack | None, topology: Topology) -> Iterator[CacheClient]:
    """A client for the topology's node: the main node as the default user, the others anonymous."""
    del live
    if topology.name == "full":
        client = CacheClient(target, credentials=target.credentials)
    elif topology.name in target.topologies:
        client = CacheClient(target, base_url=str(target.topologies[topology.name]))
    else:
        pytest.skip(f"target {target.name!r} has no {topology.name} node")
    yield client
    client.close()


def test_round_trip_on_every_protocol(node: CacheClient) -> None:
    key, body = blobs.gradle_key(), blobs.payload(10_000)
    assert node.gradle_put(key, body).status_code == 201
    assert node.gradle_get(key).content == body
    path = blobs.maven_path()
    assert node.maven_put(path, body).status_code == 201
    assert node.maven_get(path).content == body
    assert node.bazel_put("cas", sha256(body), body).status_code == 201
    assert node.bazel_get("cas", sha256(body)).content == body


def test_entry_larger_than_memory(node: CacheClient, target: Target, topology: Topology) -> None:
    """Over `memory.max_entry_size`: stored below memory, or oversized on a memory-only node."""
    key, body = blobs.gradle_key(), blobs.payload(target.memory_max_entry_size + 1024 * 1024)
    stored = node.gradle_put(key, body)
    if topology.disk or topology.bucket:
        assert stored.status_code == 201
        assert node.gradle_get(key).content == body
    else:
        assert stored.status_code == 200  # Gradle oversize: accepted and dropped
        assert node.gradle_get(key).status_code == 404
        path = blobs.maven_path()
        assert node.maven_put(path, body).status_code == 413


@pytest.mark.control
def test_restart(node: CacheClient, controlled: Stack, topology: Topology) -> None:
    """Disk or bucket keeps entries across a restart; memory alone starts empty."""
    key, body = blobs.gradle_key(), blobs.payload(10_000)
    assert node.gradle_put(key, body).status_code == 201
    controlled.restart(topology.service)  # SIGTERM flushes queued bucket uploads first
    fetched = node.gradle_get(key)
    if topology.disk or topology.bucket:
        assert fetched.content == body
    else:
        assert fetched.status_code == 404
