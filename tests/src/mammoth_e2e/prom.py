"""Queries the Prometheus that scrapes the target."""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import httpx

if TYPE_CHECKING:
    from pydantic import HttpUrl


@dataclass(frozen=True, slots=True)
class ScrapeTarget:
    job: str
    instance: str
    health: str


@dataclass(frozen=True, slots=True)
class Prometheus:
    url: HttpUrl

    def _get(self, path: str, params: dict[str, str] | None = None) -> dict[str, Any]:
        response = httpx.get(str(self.url).rstrip("/") + path, params=params, timeout=10.0)
        response.raise_for_status()
        body: dict[str, Any] = response.json()
        return body["data"]

    def targets(self) -> list[ScrapeTarget]:
        active: list[dict[str, Any]] = self._get("/api/v1/targets")["activeTargets"]
        return [
            ScrapeTarget(
                job=t["labels"]["job"], instance=t["labels"]["instance"], health=t["health"]
            )
            for t in active
        ]

    def query(self, expr: str) -> list[tuple[dict[str, str], float]]:
        result: list[dict[str, Any]] = self._get("/api/v1/query", {"query": expr})["result"]
        return [(r["metric"], float(r["value"][1])) for r in result]
