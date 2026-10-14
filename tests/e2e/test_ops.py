"""The image boots and its operational endpoints answer (docs 0001 and 0004)."""

from __future__ import annotations

import subprocess
import time
from typing import TYPE_CHECKING

import httpx
import pytest

if TYPE_CHECKING:
    from mammoth_e2e.prom import Prometheus
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target

pytestmark = pytest.mark.usefixtures("live")


@pytest.mark.parametrize("path", ["/ping", "/livez", "/readyz"])
def test_health_endpoints(target: Target, path: str) -> None:
    assert httpx.get(target.url(path)).status_code == 200


def test_unknown_route_is_aip193_not_found(target: Target) -> None:
    response = httpx.get(target.url("/no/such/route"))
    assert response.status_code == 404
    error = response.json()["error"]
    assert error["status"] == "NOT_FOUND"
    assert error["details"][0]["domain"] == "mammoth-cache.dev"


def test_metrics_pass_promtool(target: Target, controlled: Stack) -> None:
    """`promtool check metrics` accepts the exposition, as doc 0004 requires."""
    text = httpx.get(target.url("/metrics")).text
    assert text.strip(), "no metrics exported"
    assert controlled.compose.file.exists()
    result = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "promtool",
            "prom/prometheus:v3.13.4",
            "check",
            "metrics",
        ],
        input=text,
        capture_output=True,
        text=True,
        check=False,
    )
    # promtool exits 3 for lint warnings (e.g. missing HELP); only exit 1 is an invalid exposition.
    assert result.returncode in {0, 3}, result.stdout + result.stderr


def test_prometheus_scrapes_mammoth(prometheus: Prometheus) -> None:
    # Earlier tests restart the node; Prometheus reports the result of its last scrape, so give it
    # a few scrape intervals to see the node back.
    deadline = time.monotonic() + 15.0
    while True:
        targets = {(t.job, t.instance): t.health for t in prometheus.targets()}
        if targets.get(("mammoth", "mammoth:5071")) == "up" or time.monotonic() > deadline:
            break
        time.sleep(0.5)
    assert targets.get(("mammoth", "mammoth:5071")) == "up", targets
