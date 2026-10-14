"""Fixtures shared by the end-to-end and acceptance suites."""

from __future__ import annotations

import os
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from mammoth_e2e.bucket import BucketInspector
from mammoth_e2e.client import CacheClient
from mammoth_e2e.metrics import Scrape, scrape
from mammoth_e2e.prom import Prometheus
from mammoth_e2e.stack import Stack
from mammoth_e2e.target import Feature, Target

if TYPE_CHECKING:
    from collections.abc import Generator, Iterator

HERE = Path(__file__).parent
STACK_KEY = pytest.StashKey[Stack | None]()


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("mammoth")
    group.addoption(
        "--target",
        default=os.environ.get("MAMMOTH_E2E_TARGET", str(HERE / "targets" / "compose.toml")),
        help="target profile (TOML); default targets/compose.toml or $MAMMOTH_E2E_TARGET",
    )
    group.addoption(
        "--keep-stack",
        action="store_true",
        help="leave the Compose stack running after the session",
    )


@pytest.fixture(scope="session")
def target(pytestconfig: pytest.Config) -> Target:
    return Target.load(Path(str(pytestconfig.getoption("--target"))))


@pytest.fixture(scope="session")
def stack(target: Target, pytestconfig: pytest.Config) -> Iterator[Stack | None]:
    """The Compose stack, when the harness owns it; `None` for targets it doesn't."""
    if target.compose is None or not target.control:
        yield None
        return
    owned = Stack(compose=target.compose, target=target)
    owned.up()
    pytestconfig.stash[STACK_KEY] = owned
    yield owned
    if not pytestconfig.getoption("--keep-stack"):
        owned.down()


@pytest.fixture(autouse=True)
def _requirements(request: pytest.FixtureRequest, target: Target) -> None:
    """Apply the `requires(...)` and `control` markers against the target."""
    for marker in request.node.iter_markers("requires"):
        missing = [f for f in marker.args if Feature(f) not in target.features]
        if missing:
            pytest.skip(f"target {target.name!r} lacks: {', '.join(missing)}")
    if request.node.get_closest_marker("control") and not target.control:
        pytest.skip(f"target {target.name!r} does not allow control")


@pytest.fixture
def live(stack: Stack | None) -> Stack | None:
    """Request this (instead of `stack`) to make sure the stack is up before the test."""
    return stack


@pytest.fixture
def controlled(stack: Stack | None) -> Stack:
    if stack is None:
        pytest.skip("needs a stack the harness controls")
    return stack


@pytest.fixture
def client(target: Target, live: Stack | None) -> Iterator[CacheClient]:
    """A client as the target's default user."""
    del live
    c = CacheClient(target, credentials=target.credentials)
    yield c
    c.close()


@pytest.fixture
def metrics_before(target: Target, live: Stack | None) -> Scrape:
    del live
    return scrape(target)


@pytest.fixture
def open_base_url(target: Target) -> str:
    if target.open_base_url is None:
        pytest.skip(f"target {target.name!r} has no node with anonymous read")
    return str(target.open_base_url)


@pytest.fixture
def bucket(target: Target, live: Stack | None) -> BucketInspector:
    del live
    if target.bucket is None:
        pytest.skip(f"target {target.name!r} has no bucket")
    return BucketInspector.connect(target.bucket)


@pytest.fixture
def prometheus(target: Target, live: Stack | None) -> Prometheus:
    del live
    if target.prometheus_url is None:
        pytest.skip(f"target {target.name!r} has no Prometheus")
    return Prometheus(target.prometheus_url)


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(
    item: pytest.Item, call: pytest.CallInfo[None]
) -> Generator[None, pytest.TestReport, pytest.TestReport]:
    report = yield
    if report.failed and call.when == "call":
        owned = item.config.stash.get(STACK_KEY, None)
        if owned is not None:
            report.sections.append(("service logs", owned.logs()))
    return report
