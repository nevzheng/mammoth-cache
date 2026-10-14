"""Deterministic test data: cache keys, Maven paths and payloads from a seed."""

from __future__ import annotations

import hashlib
import random
import uuid


def gradle_key(seed: str = "") -> str:
    """A fresh 32-hex Gradle cache key; unique per call unless a seed is given."""
    source = seed or uuid.uuid4().hex
    return hashlib.md5(source.encode(), usedforsecurity=False).hexdigest()


def maven_path(seed: str = "") -> str:
    """A fresh path in the Maven extension's layout: v1.1/{group}/{artifact}/{checksum}/file."""
    checksum = gradle_key(seed) + gradle_key(seed + "/")
    return f"v1.1/dev.mammoth.e2e/fixture/{checksum[:16]}/buildinfo.xml"


def payload(size: int, seed: str = "") -> bytes:
    """`size` pseudo-random bytes; the same seed and size always give the same bytes."""
    return random.Random(seed or uuid.uuid4().hex).randbytes(size)  # noqa: S311 - test data
