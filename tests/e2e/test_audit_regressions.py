"""Regression tests for the persona audit of 2026-10-14, one per finding a client can see.

Each test describes the behaviour the audit asks for. Findings 1, 2, 9, 10 and 12 are fixed on
main (#46, #47); finding 4 is open, so its test is a strict xfail and the fix removes the marker.

Not here: finding 3 (a read racing an overwrite) needs a disk read to land between two syscalls,
which HTTP timing does not reach reliably, and 5, 6, 7 and 13 need faults below the HTTP surface
(a full volume, two processes on one disk, a hostile `disk.path`, a failed rename). Those belong
in the Rust tests. 8, 14 and 15 are workflow and doc findings. 11 (bucket upload order) is fixed
in #47 and has no outcome the suite can see without racing the upload queue.
"""

from __future__ import annotations

import time
from typing import TYPE_CHECKING

import httpx
import pytest

from mammoth_e2e import blobs, metrics, raw
from mammoth_e2e.client import CacheClient, auth_headers, sha256

if TYPE_CHECKING:
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

MIB = 1024 * 1024
MISS_BUDGET = 5.0  # seconds; Gradle's own timeout is 30 s, and a miss should take milliseconds


@pytest.mark.requires("disk", "bazel")
@pytest.mark.control
def test_bazel_entries_survive_a_restart_on_disk(
    client: CacheClient, target: Target, controlled: Stack
) -> None:
    """Audit 1, fixed in #46. The hits must come from disk: the bucket would hide the loss."""
    blob = blobs.payload(10_000)
    digest = sha256(blob)
    action = sha256(blobs.payload(32))
    assert client.bazel_put("cas", digest, blob).status_code == 201
    assert client.bazel_put("ac", action, b"action result").status_code == 201
    controlled.restart("mammoth")
    before = metrics.scrape(target)
    assert client.bazel_get("cas", digest).content == blob
    assert client.bazel_get("ac", action).content == b"action result"
    after = metrics.scrape(target)
    # Each disk hit is promoted to memory; a bucket hit would be promoted from the bucket.
    promoted = metrics.delta(before, after, "mammoth_tier_promotions_total", **{"from": "disk"})
    assert promoted == 2


@pytest.mark.parametrize(
    "store",
    [
        pytest.param("gradle"),
        pytest.param("cas", marks=pytest.mark.requires("bazel")),
    ],
)
def test_large_re_store_completes(client: CacheClient, target: Target, store: str) -> None:
    """Audit 2, fixed in #47. Two agents store one multi-MB output; no broken pipe."""
    body = blobs.payload(12 * MIB)
    if store == "gradle":
        path = f"/cache/{blobs.gradle_key()}"
        assert client.gradle_put(path.removeprefix("/cache/"), body).status_code == 201
    else:
        path = f"/bazel/cas/{sha256(body)}"
        assert client.bazel_put("cas", sha256(body), body).status_code == 201
    headers = auth_headers(target.credentials)
    assert raw.put_whole(str(target.base_url), path, headers, body) == 200


@pytest.mark.requires("bucket")
@pytest.mark.control
@pytest.mark.xfail(strict=True, reason="audit 4: a bucket outage stalls every local miss")
def test_bucket_outage_keeps_misses_fast(client: CacheClient, controlled: Stack) -> None:
    """A bucket that stops answering (not one that refuses connections) must not stall misses."""
    controlled.pause("seaweedfs")
    try:
        started = time.monotonic()
        try:
            miss = client.gradle_get(blobs.gradle_key())
        except httpx.TimeoutException:
            pytest.fail("a miss timed out while the bucket was down")
        assert miss.status_code == 404
        assert time.monotonic() - started < MISS_BUDGET
        started = time.monotonic()
        stored = client.gradle_put(blobs.gradle_key(), blobs.payload(1000))
        assert stored.status_code == 201
        assert time.monotonic() - started < MISS_BUDGET
    finally:
        controlled.unpause("seaweedfs")


@pytest.mark.parametrize(
    "store",
    [
        pytest.param("maven"),
        pytest.param("cas", marks=pytest.mark.requires("bazel")),
    ],
)
def test_oversize_413_reaches_the_client(target: Target, store: str) -> None:
    """Bazel never sends `Expect`; it must see the 413, not a reset mid-upload."""
    body = blobs.payload(target.max_entry_size + MIB)
    path = f"/maven/{blobs.maven_path()}" if store == "maven" else f"/bazel/cas/{sha256(body)}"
    headers = auth_headers(target.credentials)
    assert raw.put_whole(str(target.base_url), path, headers, body) == 413


@pytest.mark.requires("bucket")
@pytest.mark.control
def test_sync_upload_failure_is_counted(target: Target, controlled: Stack) -> None:
    """The node behind the proxy writes the bucket with `write = "sync"`.

    Gradle turns its cache off on a failed store, so the store still answers 201 (the audit's
    fix), but the failed upload must show in `mammoth_bucket_uploads_total{result="error"}`.
    """
    if target.proxy_url is None:
        pytest.skip(f"target {target.name!r} has no sync-bucket node")
    base = str(target.proxy_url)
    before = metrics.scrape(target, base=base)
    client = CacheClient(target, base_url=base)
    controlled.stop("seaweedfs")
    try:
        stored = client.maven_put(blobs.maven_path(), blobs.payload(1000))
        assert stored.status_code == 201
        after = metrics.scrape(target, base=base)
        assert metrics.delta(before, after, "mammoth_bucket_uploads_total", result="error") >= 1
    finally:
        client.close()
        controlled.start("seaweedfs")


@pytest.mark.requires("disk")
def test_promotions_count_only_stored_entries(client: CacheClient, target: Target) -> None:
    """A disk hit too large for memory is not promoted, so the promotions counter stays put."""
    key, body = blobs.gradle_key(), blobs.payload(target.memory_max_entry_size + 1)
    assert client.gradle_put(key, body).status_code == 201
    before = metrics.scrape(target)
    assert client.gradle_get(key).content == body
    after = metrics.scrape(target)
    assert metrics.delta(before, after, "mammoth_tier_promotions_total", to="memory") == 0
