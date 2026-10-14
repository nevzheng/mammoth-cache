"""Fixtures for the property suite."""

from __future__ import annotations

import os
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from mammoth_e2e.client import CacheClient
from mammoth_e2e.contract import Contracts
from mammoth_e2e.model import writer_credentials

if TYPE_CHECKING:
    from collections.abc import Iterator

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target


@pytest.fixture(scope="session")
def contracts() -> Contracts:
    """The contracts in this checkout, or in `$MAMMOTH_E2E_CONTRACTS` to test another build."""
    directory = os.environ.get("MAMMOTH_E2E_CONTRACTS")
    return Contracts(Path(directory)) if directory else Contracts()


@pytest.fixture
def writer(target: Target, live: Stack | None) -> Iterator[CacheClient]:
    """A client that may read and write, with credentials when the target has users."""
    del live
    c = CacheClient(target, credentials=writer_credentials(target))
    yield c
    c.close()
