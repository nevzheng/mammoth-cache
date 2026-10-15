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
from mammoth_e2e.model import AuthModel, CacheModel, ChaosModel, callers, full_access
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

EXAMPLES = int(os.environ.get("MAMMOTH_E2E_EXAMPLES", "40"))
# Each restart or outage costs seconds, so the chaos machine runs far fewer, shorter sequences.
CHAOS_EXAMPLES = int(os.environ.get("MAMMOTH_E2E_CHAOS_EXAMPLES", "6"))
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


@pytest.mark.requires("disk", "bucket")
def test_eviction_loses_nothing(target: Target, live: Stack | None, contracts: Contracts) -> None:
    """The dictionary model on `mammoth-evicting` (deploy/compose/config/evicting.toml), whose 1 MiB
    of memory and 2 MiB of disk hold a few entries in front of the bucket.

    The sequences evict entries from memory and disk and read them back from the bucket, and its
    stores wait for the upload, so the model holds as it is: every stored key hits with its bytes,
    and nothing comes back wrong.
    """
    del live
    if "evicting" not in target.topologies:
        pytest.skip(f"target {target.name!r} has no evicting node")
    # Anonymous requests may read and write there, as on the other topology nodes.
    node = target.model_copy(
        update={
            "base_url": target.topologies["evicting"],
            "users": {},
            "anonymous_scopes": frozenset(),
            "features": target.features - {Feature.AUTH, Feature.REAPI},
        }
    )

    class Machine(CacheModel):
        pass

    Machine.target, Machine.contracts = node, contracts
    Machine.owner = full_access(node)
    Machine.protocols = enabled(node)
    run_state_machine_as_test(Machine, settings=SETTINGS)


@pytest.mark.requires("disk")
@pytest.mark.control
def test_restarts_and_bucket_outages_lose_nothing(
    target: Target, controlled: Stack, contracts: Contracts
) -> None:
    """Restart the node and stop the bucket at random: every answer still matches the model."""

    class Machine(ChaosModel):
        pass

    Machine.target, Machine.contracts, Machine.stack = target, contracts, controlled
    Machine.owner = full_access(target)
    Machine.protocols = enabled(target)
    Machine.has_bucket = target.has(Feature.BUCKET)
    chaos = settings(SETTINGS, max_examples=CHAOS_EXAMPLES, stateful_step_count=20)
    run_state_machine_as_test(Machine, settings=chaos)
