"""Contract edge cases found by reading the code against the OpenAPI files, pinned as fixed tests.

The model test explores at random; these are the cases worth running every time. Known bugs are
strict xfails, so the PR that fixes one has to remove its marker. Duplicate-store draining and
racing writers are covered in test_gradle.py and test_bazel.py.
"""

from __future__ import annotations

import hashlib
import uuid
from typing import TYPE_CHECKING

import httpx
import pytest

from mammoth_e2e import blobs
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from mammoth_e2e.client import CacheClient
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target


@pytest.mark.xfail(strict=True, reason="axum's default 405 has an empty body, not AIP-193")
@pytest.mark.parametrize("method", ["POST", "DELETE", "PATCH"])
def test_wrong_method_on_gradle_is_an_aip_193_error(writer: CacheClient, method: str) -> None:
    response = writer.request(method, f"/cache/{blobs.gradle_key()}")
    assert response.status_code == 405
    assert response.headers.get("content-type", "").startswith("application/json")
    assert response.json()["error"]["code"] == 405


@pytest.mark.requires("auth")
@pytest.mark.xfail(strict=True, reason="the contract says OPTIONS needs no credentials (0001)")
def test_maven_options_needs_no_credentials(target: Target, live: Stack | None) -> None:
    del live
    if "cache:read" in target.anonymous_scopes:
        pytest.skip("anonymous can read on this target, so the case does not arise")
    response = httpx.request("OPTIONS", target.url(f"/maven/{blobs.maven_path()}"))
    assert response.status_code == 204


@pytest.mark.requires("auth")
@pytest.mark.xfail(
    strict=True, reason="auth runs before routing, so a disabled protocol answers 401 (0012)"
)
def test_disabled_bazel_looks_like_an_unknown_route(target: Target, live: Stack | None) -> None:
    """A scanner must not be able to tell a protocol that is off from one that doesn't exist."""
    del live
    if target.has(Feature.BAZEL):
        pytest.skip("Bazel is on for this target")
    digest = hashlib.sha256(uuid.uuid4().bytes).hexdigest()
    unknown = httpx.get(target.url(f"/nowhere/{digest}"))
    disabled = httpx.get(target.url(f"/bazel/ac/{digest}"))
    assert (disabled.status_code, disabled.json()) == (unknown.status_code, unknown.json())


@pytest.mark.requires("bazel")
def test_mismatched_empty_blob_is_refused(writer: CacheClient, contracts: Contracts) -> None:
    """A zero-length upload under a digest that isn't SHA-256 of nothing must not be stored."""
    digest = hashlib.sha256(uuid.uuid4().bytes).hexdigest()
    response = writer.put(f"/bazel/cas/{digest}", b"")
    assert response.status_code == 400
    contracts.check("/bazel/cas/{digest}", response)
    assert writer.get(f"/bazel/cas/{digest}").status_code == 404
