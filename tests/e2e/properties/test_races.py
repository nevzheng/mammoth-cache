"""Concurrent and broken uploads, on every protocol.

Racing writers to one key never leave a mix of bodies: an immutable key keeps exactly the body
whose store answered `201`, and a mutable key ends with one whole body. An upload that never
finishes is never served, and does not block the next store of the same key, whether it breaks
before that store starts or while it waits (design doc 0012, PRs #47 and #61).
"""

from __future__ import annotations

import os
import socket
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from typing import TYPE_CHECKING
from urllib.parse import urlsplit

import httpx
import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

from mammoth_e2e import blobs
from mammoth_e2e.client import basic
from mammoth_e2e.model import writer_credentials
from mammoth_e2e.protocols import ALL, BAZEL_CAS, GRADLE, Protocol, sha256
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

# Few examples by default: each one races real uploads of up to a few hundred kilobytes.
SETTINGS = settings(
    max_examples=int(os.environ.get("MAMMOTH_E2E_RACE_EXAMPLES", "12")),
    deadline=None,
    suppress_health_check=[HealthCheck.function_scoped_fixture, HealthCheck.too_slow],
)
# Big enough that racing uploads overlap on the wire, small enough to stay in memory.
SIZES = st.one_of(st.integers(1, 64), st.integers(50_000, 400_000))


def enabled(target: Target) -> list[Protocol]:
    return [p for p in ALL if p.feature is None or target.has(p.feature)]


def fresh_name(protocol: Protocol) -> str:
    """A key nobody has used; for CAS, callers use the digest of their body instead."""
    match protocol.name:
        case "gradle":
            return blobs.gradle_key()
        case "maven":
            return blobs.maven_path()
        case _:
            return sha256(uuid.uuid4().bytes)


def headers(target: Target, protocol: Protocol) -> dict[str, str]:
    found: dict[str, str] = {}
    if target.has(Feature.AUTH) and (creds := writer_credentials(target)) is not None:
        found["Authorization"] = basic(creds)
    if protocol == GRADLE:
        found["Content-Type"] = "application/vnd.gradle.build-cache-artifact.v2"
    return found


def client(target: Target) -> httpx.Client:
    return httpx.Client(base_url=target.url("/"), timeout=httpx.Timeout(30.0))


def race(target: Target, protocol: Protocol, path: str, bodies: list[bytes]) -> list[int]:
    """Stores every body under `path` at once, each on its own connection; their statuses."""
    start = threading.Barrier(len(bodies))
    sent = headers(target, protocol)

    def put(body: bytes) -> int:
        with client(target) as http:
            start.wait()
            return http.put(path, content=body, headers=sent).status_code

    with ThreadPoolExecutor(len(bodies)) as pool:
        return list(pool.map(put, bodies))


def read(target: Target, protocol: Protocol, path: str) -> httpx.Response:
    with client(target) as http:
        return http.get(path, headers=headers(target, protocol))


@pytest.fixture
def writable(target: Target, live: Stack | None) -> Target:
    """The target, with its stack up. Hypothesis can't take `live` itself, as it is unused."""
    del live
    return target


@given(data=st.data(), count=st.integers(2, 6))
@SETTINGS
def test_racing_stores_keep_one_whole_body(
    writable: Target, contracts: Contracts, data: st.DataObject, count: int
) -> None:
    target = writable
    protocol = data.draw(st.sampled_from(enabled(target)), label="protocol")
    seed = uuid.uuid4().hex
    if protocol == BAZEL_CAS:
        # One digest, one body: every racer stores the same bytes.
        body = blobs.payload(data.draw(SIZES, label="size"), seed)
        bodies = [body] * count
        path = protocol.path(sha256(body))
    else:
        sizes = data.draw(st.lists(SIZES, min_size=count, max_size=count), label="sizes")
        bodies = [blobs.payload(size, f"{seed}/{i}") for i, size in enumerate(sizes)]
        path = protocol.path(fresh_name(protocol))

    statuses = race(target, protocol, path, bodies)

    got = read(target, protocol, path)
    contracts.check(protocol.template, got)
    assert got.status_code == 200, f"{path}: stored by {count} racers, answered {got.status_code}"
    if protocol.mutable:
        assert statuses == [201] * count, f"{path}: racing stores answered {statuses}"
        assert got.content in bodies, f"{path}: serves {len(got.content)} bytes nobody stored"
    else:
        # The first store writes; the others wait for it and answer 200 (#61).
        assert sorted(statuses) == [200] * (count - 1) + [201], (
            f"{path}: racing stores of an immutable key answered {statuses}"
        )
        winner = bodies[statuses.index(201)]
        assert got.content == winner, f"{path}: serves a body other than the one stored"
    # Every later read, on a new connection, agrees.
    assert read(target, protocol, path).content == got.content


def broken_upload(
    target: Target, protocol: Protocol, path: str, size: int, sent: int
) -> socket.socket:
    """Starts a `PUT` of `size` bytes and sends only the first `sent`; close it to break it."""
    url = urlsplit(target.url("/"))
    assert url.hostname is not None
    conn = socket.create_connection((url.hostname, url.port or 80), timeout=10)
    lines = [f"PUT {path} HTTP/1.1", f"Host: {url.netloc}", f"Content-Length: {size}"]
    lines += [f"{name}: {value}" for name, value in headers(target, protocol).items()]
    conn.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
    conn.sendall(blobs.payload(size, path)[:sent])
    return conn


@given(data=st.data())
@SETTINGS
def test_broken_upload_is_never_served_and_blocks_nothing(
    writable: Target, contracts: Contracts, data: st.DataObject
) -> None:
    size = data.draw(st.integers(2, 300_000), label="size")
    sent = int(size * data.draw(st.floats(0.0, 0.99), label="cut"))
    # Whether the upload breaks before the whole store starts, or while that store waits on it.
    while_waiting = data.draw(st.booleans(), label="while_waiting")
    target = writable
    if urlsplit(target.url("/")).scheme != "http":
        pytest.skip("sends raw HTTP/1.1, so needs a plain HTTP target")
    protocol = data.draw(st.sampled_from(enabled(target)), label="protocol")
    body = blobs.payload(size, uuid.uuid4().hex)
    path = protocol.path(sha256(body) if protocol == BAZEL_CAS else fresh_name(protocol))

    broken = broken_upload(target, protocol, path, size, sent)
    if not while_waiting:
        broken.close()
        time.sleep(0.1)  # let the node see the connection go
        missing = read(target, protocol, path)
        contracts.check(protocol.template, missing)
        assert missing.status_code == 404, f"{path}: a broken upload answered {missing.status_code}"

    # A whole store of the same key: it must not hang behind the broken one, and must stick.
    with ThreadPoolExecutor(1) as pool, client(target) as http:
        store = pool.submit(http.put, path, content=body, headers=headers(target, protocol))
        if while_waiting:
            time.sleep(0.3)
            broken.close()
        response = store.result(timeout=30)
    contracts.check(protocol.template, response)
    assert response.status_code == 201, (
        f"{path}: store after a broken one answered {response.status_code}"
    )
    got = read(target, protocol, path)
    assert got.status_code == 200
    assert got.content == body, f"{path}: serves {len(got.content)} bytes, stored {size}"
