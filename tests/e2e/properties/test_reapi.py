"""The gRPC (REAPI) and HTTP Bazel caches are one cache, for any sequence of calls (doc 0016)."""

from __future__ import annotations

import os
from typing import TYPE_CHECKING

import pytest
from hypothesis import HealthCheck, settings
from hypothesis.stateful import run_state_machine_as_test

from mammoth_e2e.client import basic
from mammoth_e2e.model import full_access, writer_credentials
from mammoth_e2e.parity import ParityModel
from mammoth_e2e.target import Feature

if TYPE_CHECKING:
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
