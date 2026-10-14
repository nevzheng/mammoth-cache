"""The node behaves like a dictionary, on every protocol, for any sequence of requests.

Hypothesis drives `PUT`, `GET` and `HEAD` across Gradle, Maven and (when on) Bazel's AC and CAS,
reuses names across protocols, and checks every answer against a model and the OpenAPI contracts.
A failure prints the shortest sequence of requests that breaks the node.
"""

from __future__ import annotations

import os
from typing import TYPE_CHECKING

import pytest
from hypothesis import HealthCheck, settings
from hypothesis.stateful import run_state_machine_as_test

from mammoth_e2e import protocols
from mammoth_e2e.model import AuthModel, CacheModel, callers, full_access
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

EXAMPLES = int(os.environ.get("MAMMOTH_E2E_EXAMPLES", "40"))
SETTINGS = settings(
    max_examples=EXAMPLES,
    stateful_step_count=30,
    deadline=None,
    suppress_health_check=[HealthCheck.too_slow, HealthCheck.data_too_large],
)


def enabled(target: Target) -> tuple[protocols.Protocol, ...]:
    return tuple(p for p in protocols.ALL if p.feature is None or target.has(p.feature))


def test_cache_behaves_like_a_dict(
    target: Target, live: Stack | None, contracts: Contracts
) -> None:
    del live
    if target.has(Feature.AUTH):
        owner = full_access(target)
    else:
        owner = full_access(
            target.model_copy(update={"users": {}, "anonymous_scopes": frozenset()})
        )

    class Machine(CacheModel):
        pass

    Machine.target, Machine.contracts, Machine.owner = target, contracts, owner
    Machine.protocols = enabled(target)
    run_state_machine_as_test(Machine, settings=SETTINGS)


@pytest.mark.requires("auth")
def test_access_follows_scopes(target: Target, live: Stack | None, contracts: Contracts) -> None:
    """Every caller gets exactly what its scopes allow, and a refused store changes nothing."""
    del live

    class Machine(AuthModel):
        pass

    Machine.target, Machine.contracts = target, contracts
    Machine.owner = full_access(target)
    Machine.everyone = tuple(callers(target))
    Machine.protocols = enabled(target)
    run_state_machine_as_test(Machine, settings=SETTINGS)
