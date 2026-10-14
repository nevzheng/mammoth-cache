"""Client version matrix for the acceptance suite (doc 0008).

Versions are lines, resolved to their newest release when the suite runs: `7.6` is the newest
7.6.x, `9` the newest 9.x, `latest` the newest release of any line.
"""

from __future__ import annotations

import os
from typing import TYPE_CHECKING

from mammoth_e2e import versions

if TYPE_CHECKING:
    import pytest


def _split(value: str) -> list[str]:
    return [v.strip() for v in value.split(",") if v.strip()]


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("mammoth acceptance")
    group.addoption(
        "--gradle",
        default=os.environ.get("MAMMOTH_E2E_GRADLE", "7.6,8.14,9"),
        help="Gradle version lines, comma-separated (default: 7.6,8.14,9)",
    )
    group.addoption(
        "--maven",
        default=os.environ.get("MAMMOTH_E2E_MAVEN", "3.9"),
        help="Maven version lines (default: 3.9)",
    )
    group.addoption(
        "--maven-extension",
        default=os.environ.get("MAMMOTH_E2E_MAVEN_EXTENSION", "1.2,latest"),
        help="Maven Build Cache Extension version lines (default: 1.2,latest)",
    )


def pytest_generate_tests(metafunc: pytest.Metafunc) -> None:
    config = metafunc.config
    if "gradle_version" in metafunc.fixturenames:
        resolved = sorted(
            {versions.gradle(line) for line in _split(str(config.getoption("--gradle")))}
        )
        metafunc.parametrize("gradle_version", resolved, scope="module")
    if "maven_versions" in metafunc.fixturenames:
        mavens = {versions.maven(line) for line in _split(str(config.getoption("--maven")))}
        extensions = {
            versions.maven_extension(line)
            for line in _split(str(config.getoption("--maven-extension")))
        }
        pairs = sorted((m, e) for m in mavens for e in extensions)
        metafunc.parametrize(
            "maven_versions", pairs, ids=[f"maven{m}-ext{e}" for m, e in pairs], scope="module"
        )
