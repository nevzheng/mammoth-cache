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

from mammoth_e2e.client import basic
from mammoth_e2e.model import full_access, writer_credentials
from mammoth_e2e.parity import ParityModel
from mammoth_e2e.reapi import Reapi, attempt
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
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
    writer = writer_credentials(target) if target.has(Feature.AUTH) else None

    class Machine(ParityModel):
        pass

    Machine.target, Machine.contracts = target, contracts
    Machine.owner = full_access(target)
    Machine.authorization = basic(writer) if writer is not None else None
    run_state_machine_as_test(
        Machine,
        settings=settings(
            max_examples=EXAMPLES,
            stateful_step_count=30,
            deadline=None,
            suppress_health_check=[HealthCheck.too_slow, HealthCheck.data_too_large],
        ),
    )


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
        pytest.param(
            "tree file",
            marks=pytest.mark.xfail(
                strict=True, reason="post-re-audit finding 8: trees not walked"
            ),
        ),
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
