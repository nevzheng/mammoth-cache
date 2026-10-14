"""The gRPC (REAPI) and HTTP Bazel caches are one cache, for any sequence of calls (doc 0016)."""

from __future__ import annotations

import os
import time
import uuid
from typing import TYPE_CHECKING

import grpc
import httpx
import pytest
from hypothesis import HealthCheck, settings
from hypothesis.stateful import run_state_machine_as_test

from mammoth_e2e import metrics
from mammoth_e2e.client import basic
from mammoth_e2e.model import full_access, writer_credentials
from mammoth_e2e.parity import ParityModel
from mammoth_e2e.reapi import Reapi, attempt
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from collections.abc import Callable

    from mammoth_e2e.bucket import BucketInspector
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

EXAMPLES = int(os.environ.get("MAMMOTH_E2E_EXAMPLES", "40"))


@pytest.mark.requires("bazel", "reapi")
def test_grpc_and_http_share_one_cache(
    target: Target, live: Stack | None, contracts: Contracts
) -> None:
    del live
    run_state_machine_as_test(
        _machine(target, contracts),
        settings=settings(
            max_examples=EXAMPLES,
            stateful_step_count=30,
            deadline=None,
            suppress_health_check=[HealthCheck.too_slow, HealthCheck.data_too_large],
        ),
    )


def _machine(target: Target, contracts: Contracts) -> type[ParityModel]:
    writer = writer_credentials(target) if target.has(Feature.AUTH) else None

    class Machine(ParityModel):
        pass

    Machine.target, Machine.contracts = target, contracts
    Machine.owner = full_access(target)
    Machine.authorization = basic(writer) if writer is not None else None
    return Machine


def _unreadable_tree(m: ParityModel) -> None:
    """An output directory whose tree blob is not a `Tree` (a 1-byte blob) is still a hit."""
    action = m.first_action(seed="")
    empty = m.first_blob(size=0, seed="0000")
    m.start()
    m.batch_read_many(found=[empty])
    tree = m.new_blob(seed="0000", size=1)
    m.batch_upload(corrupt=set(), drawn=[tree])
    m.store_result(action=action, exit_code=0, outputs=[], stdout=None, trees=[tree], way="http")
    m.get_result(action=action, way="grpc")


def _empty_blob_over_http(m: ParityModel) -> None:
    """The empty blob reads over HTTP although nobody stored it."""
    m.first_action(seed="")
    empty = m.first_blob(size=0, seed="0000")
    m.start()
    m.batch_read_many(found=[empty])
    m.batch_upload_too_big(seed="0000")
    m.read(blob=empty, limit=1_791_652_643_733_496_121_956_237_312, offset=1675, way="http")


def _batch_read_past_the_advertised_limit(m: ParityModel) -> None:
    """A batch read one byte past max_batch_total_size_bytes is refused, though it would fit."""
    m.first_action(seed="")
    empty = m.first_blob(size=0, seed="0000")
    m.start()
    m.batch_read_many(found=[empty])
    m.batch_read_too_big(claimed=3_932_161, found=[])


@pytest.mark.requires("bazel", "reapi")
@pytest.mark.parametrize(
    "steps",
    [_unreadable_tree, _empty_blob_over_http, _batch_read_past_the_advertised_limit],
    ids=lambda f: f.__name__.strip("_"),
)
def test_found_examples(
    target: Target, live: Stack | None, contracts: Contracts, steps: Callable[[ParityModel], None]
) -> None:
    """Sequences random runs of `test_grpc_and_http_share_one_cache` once failed on (#114),
    replayed on every run: the derandomized CI profile would not draw them."""
    del live
    machine = _machine(target, contracts)()
    try:
        steps(machine)
    finally:
        machine.teardown()


def _bucket_up(target: Target, client: Reapi) -> None:
    """Waits out a bucket cooldown left by earlier tests: while the node skips the bucket, its
    background uploads are dropped and nothing written now would ever land there."""
    deadline = time.monotonic() + 60  # the 30 s cooldown, then a probe on the next lookup
    while metrics.scrape(target).value("mammoth_tier_up", tier="bucket") != 1:
        assert time.monotonic() < deadline, "the bucket tier never came back up"
        client.find_missing([client.digest(uuid.uuid4().bytes)])  # a lookup probes the bucket
        time.sleep(1)


def _landed(bucket: BucketInspector, suffix: str) -> list[str]:
    """The bucket keys for `suffix`, once the node's background upload has written them."""
    deadline = time.monotonic() + 30
    while not (keys := [k for k in bucket.list_keys() if k.endswith(suffix)]):
        assert time.monotonic() < deadline, f"{suffix} never reached the bucket"
        time.sleep(0.5)
    return keys


def _evict(stack: Stack, bucket: BucketInspector, blob: str, action: str) -> None:
    """Removes a CAS blob from every tier of `mammoth`, keeping the action result: memory and
    disk go by restarting the node on an empty disk, then the result comes back from the bucket."""
    _landed(bucket, f"bazel-ac/{action}")
    keys = _landed(bucket, f"bazel-cas/{blob}")
    stack.stop("mammoth")
    stack.wipe_disk("mammoth")
    for key in keys:
        bucket.delete(key)
    stack.start("mammoth")
    stack.wait_ready()


@pytest.mark.control
@pytest.mark.requires("bazel", "reapi", "bucket")
@pytest.mark.parametrize(
    "output",
    [
        "file",
        "tree",
        "tree file",
    ],
)
def test_evicted_output_is_a_miss(
    target: Target, controlled: Stack, bucket: BucketInspector, output: str
) -> None:
    """A result whose output (a file, a tree, or a file in a tree) was evicted from every tier is
    a miss, so build without the bytes never trusts it (doc 0016)."""
    assert target.reapi_address is not None
    writer = writer_credentials(target) if target.has(Feature.AUTH) else None
    client = Reapi(target.reapi_address, basic(writer) if writer is not None else None)
    re = client.s.re
    salt = uuid.uuid4().hex.encode()
    file = salt + b"/file"
    tree = re.Tree(root=re.Directory(files=[re.FileNode(name="f", digest=client.digest(file))]))
    tree_bytes = tree.SerializeToString(deterministic=True)
    _bucket_up(target, client)
    assert client.batch_update(
        [(client.digest(file), file), (client.digest(tree_bytes), tree_bytes)]
    ) == [0, 0]
    result = re.ActionResult(exit_code=0)
    if output == "file":
        result.output_files.add(path="out", digest=client.digest(file))
    else:
        result.output_directories.add(path="dir", tree_digest=client.digest(tree_bytes))
    action = client.digest(salt + b"/action")
    client.update_action_result(action, result)
    assert client.get_action_result(action) == result
    evicted = client.digest(tree_bytes if output == "tree" else file).hash
    _evict(controlled, bucket, evicted, action.hash)
    client.close()
    client = Reapi(target.reapi_address, client.authorization)
    owner = full_access(target)
    stored = httpx.get(
        target.url(f"/bazel/ac/{action.hash}"), headers=owner.headers, auth=owner.auth, timeout=10
    )
    assert stored.status_code == 200, "the action result itself must survive the eviction"
    try:
        code, _ = attempt(lambda: client.get_action_result(action))
        assert code == grpc.StatusCode.NOT_FOUND
    finally:
        client.close()
