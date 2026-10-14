"""Synthetic build-cache traffic: which entries a fleet of builds asks for, and how big they are.

Builds share most of their cache keys, and a few keys (core libraries, codegen) are fetched far
more than the rest, so keys follow a Zipf distribution. Entry sizes are log-normal: mostly small
class-file archives, with a long tail of large ones. Everything derives from a seed, so the same
model gives the same keys and sizes on every run and every machine.
"""

from __future__ import annotations

import hashlib
import itertools
import math
import random
from dataclasses import dataclass, field

_BUFFER_SIZE = 64 * 1024 * 1024


@dataclass(frozen=True, slots=True)
class Model:
    """The shape of the traffic. The defaults resemble a mid-sized Gradle monorepo's CI."""

    keys: int = 20_000  # distinct entries the fleet builds
    zipf_s: float = 1.1  # skew: higher means a few keys take more of the reads
    churn: float = 0.05  # share of requests for a key no build has made before (changed inputs)
    size_median: int = 16 * 1024  # bytes
    size_sigma: float = 1.5  # log-normal spread; 1.5 puts the 99th percentile near 0.5 MiB
    size_max: int = 8 * 1024 * 1024  # keep under the target's memory.max_entry_size
    seed: str = "mammoth-load"


@dataclass(slots=True)
class Traffic:
    """Draws keys and builds bodies for one simulated client. Not thread-safe; one per user."""

    model: Model
    rng: random.Random = field(init=False)
    _cum_weights: list[float] = field(init=False)
    _population: range = field(init=False)
    _buffer: bytes = field(init=False)

    def __post_init__(self) -> None:
        self.rng = random.Random()  # noqa: S311 - traffic, not secrets
        weights = (1.0 / (rank**self.model.zipf_s) for rank in range(1, self.model.keys + 1))
        self._cum_weights = list(itertools.accumulate(weights))
        self._population = range(self.model.keys)
        self._buffer = _shared_buffer(self.model.seed)

    def rank(self) -> int:
        """A key rank: 0 is the most requested; ranks at or past `keys` are new keys."""
        if self.rng.random() < self.model.churn:
            return self.model.keys + self.rng.getrandbits(48)
        return self.rng.choices(self._population, cum_weights=self._cum_weights)[0]

    def size(self, rank: int) -> int:
        """The entry's size, fixed by its rank."""
        draw = random.Random(f"{self.model.seed}/size/{rank}").gauss(0.0, self.model.size_sigma)  # noqa: S311
        return max(1, min(self.model.size_max, round(self.model.size_median * math.exp(draw))))

    def gradle_key(self, rank: int) -> str:
        return hashlib.md5(f"{self.model.seed}/{rank}".encode(), usedforsecurity=False).hexdigest()

    def maven_path(self, rank: int) -> str:
        checksum = hashlib.sha256(f"{self.model.seed}/maven/{rank}".encode()).hexdigest()[:16]
        return f"v1.1/dev.mammoth.load/module-{rank % 500}/{checksum}/buildinfo.xml"

    def body(self, rank: int) -> bytes:
        """The entry's bytes: the same for a rank every time, distinct between ranks."""
        size = self.size(rank)
        prefix = f"{self.model.seed}/{rank}\n".encode()
        offset = random.Random(f"{self.model.seed}/offset/{rank}").randrange(_BUFFER_SIZE - size)  # noqa: S311
        return (prefix + self._buffer[offset : offset + size])[:size]


_buffers: dict[str, bytes] = {}


def _shared_buffer(seed: str) -> bytes:
    """Random bytes that bodies are sliced from, made once per process."""
    if seed not in _buffers:
        _buffers[seed] = random.Random(seed).randbytes(_BUFFER_SIZE)  # noqa: S311
    return _buffers[seed]
