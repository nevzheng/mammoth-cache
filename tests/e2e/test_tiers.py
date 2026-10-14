"""Storage tiers against real infrastructure: disk volume and S3 bucket (docs 0003, 0011)."""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e import blobs

if TYPE_CHECKING:
    from mammoth_e2e.bucket import BucketInspector
    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.stack import Stack


def test_bucket_is_reachable(bucket: BucketInspector) -> None:
    """SeaweedFS is up and the bucket exists, whether or not mammoth uses it yet."""
    assert bucket.exists()


@pytest.mark.requires("disk")
@pytest.mark.control
def test_entries_survive_a_restart(client: CacheClient, controlled: Stack) -> None:
    key, body = blobs.gradle_key(), blobs.payload(1000)
    assert client.gradle_put(key, body).status_code == 201
    controlled.restart("mammoth")
    assert client.gradle_get(key).content == body


@pytest.mark.requires("bucket")
def test_write_through_reaches_the_bucket(client: CacheClient, bucket: BucketInspector) -> None:
    key, body = blobs.gradle_key(), blobs.payload(1000)
    assert client.gradle_put(key, body).status_code == 201
    # Once the bucket tier exports it, wait for mammoth_bucket_upload_queue_entries to reach 0.
    assert any(k.endswith(f"gradle/{key}") for k in bucket.list_keys())


@pytest.mark.requires("bucket")
@pytest.mark.control
def test_bucket_down_keeps_serving(client: CacheClient, controlled: Stack) -> None:
    key, body = blobs.gradle_key(), blobs.payload(1000)
    assert client.gradle_put(key, body).status_code == 201
    controlled.stop("seaweedfs")
    try:
        assert client.gradle_get(key).content == body
        assert client.gradle_put(blobs.gradle_key(), body).status_code < 500
    finally:
        controlled.start("seaweedfs")
