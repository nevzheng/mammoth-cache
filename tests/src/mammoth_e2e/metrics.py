"""Scrapes `/metrics` and compares counters between two scrapes."""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

import httpx
from prometheus_client.parser import text_string_to_metric_families

if TYPE_CHECKING:
    from collections.abc import Mapping

    from mammoth_e2e.target import Target

type Labels = frozenset[tuple[str, str]]


@dataclass(frozen=True, slots=True)
class Scrape:
    """One scrape: sample name and labels to value."""

    text: str
    samples: Mapping[tuple[str, Labels], float]

    def value(self, name: str, **labels: str) -> float:
        """Sum of every sample of `name` whose labels include `labels`."""
        want = set(labels.items())
        return sum(v for (n, ls), v in self.samples.items() if n == name and want <= ls)

    def names(self) -> set[str]:
        return {n for n, _ in self.samples}


def parse(text: str) -> Scrape:
    samples: dict[tuple[str, Labels], float] = {}
    for family in text_string_to_metric_families(text):
        for sample in family.samples:
            samples[sample.name, frozenset(sample.labels.items())] = sample.value
    return Scrape(text=text, samples=samples)


def scrape(target: Target, *, base: str | None = None) -> Scrape:
    response = httpx.get(target.url("/metrics", base=base), timeout=10.0)
    response.raise_for_status()
    return parse(response.text)


def delta(before: Scrape, after: Scrape, name: str, **labels: str) -> float:
    return after.value(name, **labels) - before.value(name, **labels)
