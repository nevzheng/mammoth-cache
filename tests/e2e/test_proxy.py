"""Large stores through a reverse proxy (doc 0013): the settings the guide gives, and the trap.

nginx and ingress-nginx refuse bodies over 1 MiB by default with `413`, and Gradle turns its
remote cache off for the build on any failed store. The guide says to lift the limit; these tests
send a 100 MiB store through nginx with and without that setting.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from mammoth_e2e import blobs
from mammoth_e2e.client import CacheClient, sha256

if TYPE_CHECKING:
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

LARGE = 100 * 1024 * 1024


@pytest.fixture
def proxy_url(target: Target, live: Stack | None) -> str:
    del live
    if target.proxy_url is None:
        pytest.skip(f"target {target.name!r} has no proxy")
    return str(target.proxy_url)


@pytest.fixture
def proxy_defaults_url(target: Target, live: Stack | None) -> str:
    del live
    if target.proxy_defaults_url is None:
        pytest.skip(f"target {target.name!r} has no proxy on its defaults")
    return str(target.proxy_defaults_url)


def test_large_store_through_a_configured_proxy(target: Target, proxy_url: str) -> None:
    client = CacheClient(target, base_url=proxy_url)
    key, body = blobs.gradle_key(), blobs.payload(LARGE)
    try:
        stored = client.gradle_put(key, body)
        assert stored.status_code == 201, stored.text
        fetched = client.gradle_get(key)
        assert fetched.status_code == 200
        assert sha256(fetched.content) == sha256(body)
    finally:
        client.close()


def test_large_store_through_proxy_defaults_is_refused(
    target: Target, proxy_defaults_url: str
) -> None:
    """The trap the guide warns about: the proxy, not mammoth, answers 413."""
    client = CacheClient(target, base_url=proxy_defaults_url)
    key = blobs.gradle_key()
    try:
        stored = client.gradle_put(key, blobs.payload(LARGE))
        assert stored.status_code == 413
        assert stored.headers.get("server", "").startswith("nginx")
        assert "application/json" not in stored.headers.get("content-type", "")
        assert client.gradle_get(key).status_code == 404
    finally:
        client.close()
