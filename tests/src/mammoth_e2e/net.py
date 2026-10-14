"""HTTP downloads from public registries, retried on rate limits and server errors."""

from __future__ import annotations

import time
from contextlib import contextmanager
from typing import TYPE_CHECKING

import httpx

if TYPE_CHECKING:
    from collections.abc import Generator

_RETRY_STATUSES = frozenset({429, 500, 502, 503, 504})


def _wait(attempt: int, response: httpx.Response | None) -> float:
    retry_after = response.headers.get("retry-after", "") if response is not None else ""
    return float(retry_after) if retry_after.isdigit() else min(2.0**attempt, 30.0)


@contextmanager
def stream(url: str, *, attempts: int = 6, timeout: float = 120.0) -> Generator[httpx.Response]:
    """GET `url` as a stream, retrying 429 and 5xx with backoff before yielding a 2xx."""
    for attempt in range(attempts):
        last = attempt == attempts - 1
        try:
            with httpx.stream("GET", url, follow_redirects=True, timeout=timeout) as response:
                if response.status_code in _RETRY_STATUSES and not last:
                    delay = _wait(attempt, response)
                else:
                    response.raise_for_status()
                    yield response
                    return
        except httpx.TransportError:
            if last:
                raise
            delay = _wait(attempt, None)
        time.sleep(delay)
    raise AssertionError("unreachable")


def get_text(url: str) -> str:
    with stream(url, timeout=30.0) as response:
        return response.read().decode()
