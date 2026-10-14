"""Property: any build graph, rebuilt from scratch, is served whole from the cache (doc 0016).

Hypothesis generates small genrule graphs, with outputs from empty through past the 4 MiB gRPC
batch limit, and a sequence of clean builds, each over gRPC or the HTTP cache and with any
`--remote_download_*` mode. The first build executes every action; every later one must be all
remote hits with byte-identical outputs, whichever protocol stored the entries. Each example
runs real Bazel, so there are few: `MAMMOTH_E2E_BAZEL_EXAMPLES` (default 4).
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import TYPE_CHECKING

import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

from mammoth_e2e.builds import BazelProject

if TYPE_CHECKING:
    from mammoth_e2e.builds import Download
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Credentials, Target

pytestmark = [pytest.mark.requires("reapi"), pytest.mark.requires("bazel")]

BATCH_LIMIT = 4 * 1024 * 1024 - 256 * 1024
SIZES = st.sampled_from([0, 1, 4096, 65_536, BATCH_LIMIT - 4096, BATCH_LIMIT + 1, 6 * 1024 * 1024])
EXAMPLES = int(os.environ.get("MAMMOTH_E2E_BAZEL_EXAMPLES", "4"))


@dataclass(frozen=True, slots=True)
class Blob:
    size: int
    deps: tuple[int, ...]  # indexes of earlier blobs


@dataclass(frozen=True, slots=True)
class Build:
    protocol: str  # "grpc" or "http"
    download: Download


@st.composite
def graphs(draw: st.DrawFn) -> list[Blob]:
    blobs: list[Blob] = []
    for i in range(draw(st.integers(min_value=1, max_value=5))):
        deps = draw(st.sets(st.integers(min_value=0, max_value=i - 1), max_size=2)) if i else set()
        blobs.append(Blob(size=draw(SIZES), deps=tuple(sorted(deps))))
    return blobs


BUILDS = st.builds(
    Build,
    protocol=st.sampled_from(["grpc", "http"]),
    download=st.sampled_from(["all", "toplevel", "minimal"]),
)


@dataclass(frozen=True, slots=True)
class Setup:
    version: str
    urls: dict[str, str]  # by protocol
    credentials: Credentials | None


@pytest.fixture
def setup(bazel_version: str, target: Target, live: Stack | None) -> Setup:
    del live
    if target.reapi_address is None:
        pytest.skip("target has no REAPI address")
    urls = {"grpc": f"grpc://{target.reapi_address}", "http": target.url("/bazel")}
    return Setup(bazel_version, urls, target.credentials)


@settings(
    max_examples=EXAMPLES,
    deadline=None,
    suppress_health_check=[HealthCheck.function_scoped_fixture, HealthCheck.too_slow],
)
@given(blobs=graphs(), builds=st.lists(BUILDS, min_size=2, max_size=3))
def test_rebuild_is_all_hits(
    tmp_path_factory: pytest.TempPathFactory, setup: Setup, blobs: list[Blob], builds: list[Build]
) -> None:
    workdir = tmp_path_factory.mktemp("graph")
    project = BazelProject.create(workdir, setup.version, setup.urls["grpc"])
    project.credentials = setup.credentials
    labels = [
        project.add_blob(f"b{i}", b.size, tuple(f"//gen:b{d}" for d in b.deps))
        for i, b in enumerate(blobs)
    ]
    reference: dict[str, bytes] | None = None
    for n, build in enumerate(builds):
        project.cache_url = setup.urls[build.protocol]
        project.download = build.download
        result = project.build(*labels)
        assert result.ok, result.describe()
        if n == 0:
            assert result.remote_hits() == 0, result.describe()
            assert result.executed() == len(blobs), result.describe()
        else:
            assert result.remote_hits() == len(blobs), result.describe()
            assert result.executed() == 0, result.describe()
        if build.download != "minimal":
            outputs = project.outputs()
            if reference is None:
                reference = outputs
            # toplevel downloads only what was asked for, which is every label here.
            assert outputs == reference
