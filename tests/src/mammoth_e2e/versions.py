"""Resolves a client version line (`7.6`, `9`, `latest`) to its newest release at run time.

Doc 0008 tests "the latest patch release of each line, resolved when the suite runs".
"""

from __future__ import annotations

import json
import re
import subprocess
import xml.etree.ElementTree as ET
from functools import cache
from typing import Any

from mammoth_e2e import net

GRADLE_VERSIONS = "https://services.gradle.org/versions/all"
MAVEN_CENTRAL = "https://repo1.maven.org/maven2"
MAVEN_DIST = "org/apache/maven/apache-maven"
MAVEN_EXTENSION = "org/apache/maven/extensions/maven-build-cache-extension"

_RELEASE = re.compile(r"^\d+(\.\d+)*$")


def _key(version: str) -> tuple[int, ...]:
    return tuple(int(p) for p in version.split("."))


def _newest_in_line(versions: list[str], line: str) -> str:
    """The newest release in `versions` that is `line` or starts with `line.`."""
    releases = [v for v in versions if _RELEASE.match(v)]
    if line != "latest":
        releases = [v for v in releases if v == line or v.startswith(line + ".")]
    if not releases:
        raise LookupError(f"no release matches {line!r}")
    return max(releases, key=_key)


@cache
def _gradle_releases() -> list[str]:
    entries: list[dict[str, Any]] = json.loads(net.get_text(GRADLE_VERSIONS))
    return [str(e["version"]) for e in entries if not e["snapshot"] and not e["broken"]]


@cache
def _maven_metadata(path: str) -> list[str]:
    root = ET.fromstring(net.get_text(f"{MAVEN_CENTRAL}/{path}/maven-metadata.xml"))  # noqa: S314 - Maven Central over TLS
    return [v.text for v in root.iter("version") if v.text]


def gradle(line: str) -> str:
    return line if line.count(".") >= 2 else _newest_in_line(_gradle_releases(), line)


def maven(line: str) -> str:
    return line if line.count(".") >= 2 else _newest_in_line(_maven_metadata(MAVEN_DIST), line)


def maven_extension(line: str) -> str:
    if line.count(".") >= 2:
        return line
    return _newest_in_line(_maven_metadata(MAVEN_EXTENSION), line)


BAZEL_REPOSITORY = "https://github.com/bazelbuild/bazel"


@cache
def _bazel_releases() -> list[str]:
    # git ls-remote has no API rate limit, unlike the GitHub REST API.
    out = subprocess.run(
        ["git", "ls-remote", "--tags", "--refs", BAZEL_REPOSITORY],
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
    ).stdout
    return [line.rpartition("refs/tags/")[2] for line in out.splitlines()]


def bazel(line: str) -> str:
    return line if line.count(".") >= 2 else _newest_in_line(_bazel_releases(), line)


BUCK2_REPOSITORY = "https://github.com/facebook/buck2"
_BUCK2_RELEASE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


@cache
def _buck2_releases() -> list[str]:
    out = subprocess.run(
        ["git", "ls-remote", "--tags", "--refs", BUCK2_REPOSITORY],
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
    ).stdout
    tags = (line.rpartition("refs/tags/")[2] for line in out.splitlines())
    return [t for t in tags if _BUCK2_RELEASE.match(t)]


def buck2(line: str) -> str:
    """Buck2 releases are dated (`2026-10-01`); `latest` is the newest, a date pins one."""
    if line != "latest":
        return line
    return max(_buck2_releases())
