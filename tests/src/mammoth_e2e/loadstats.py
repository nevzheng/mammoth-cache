"""Serves a Locust run's client-side numbers in the Prometheus text format.

Prometheus scrapes this next to mammoth's `/metrics`, so one query or dashboard shows both sides
of a run. The numbers are Locust's own: counters since the run started and the response-time
percentiles of its last few seconds, per request name.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, override

from prometheus_client import start_http_server
from prometheus_client.core import REGISTRY, CounterMetricFamily, GaugeMetricFamily
from prometheus_client.registry import Collector

if TYPE_CHECKING:
    from collections.abc import Iterator

    from locust.env import Environment
    from prometheus_client.metrics_core import Metric

QUANTILES = (0.5, 0.9, 0.99, 0.999)


class LocustCollector(Collector):
    def __init__(self, environment: Environment) -> None:
        self.environment = environment

    @override
    def collect(self) -> Iterator[Metric]:
        labels = ["name", "method"]
        requests = CounterMetricFamily("locust_requests", "Requests sent", labels=labels)
        failures = CounterMetricFamily("locust_failures", "Requests that failed", labels=labels)
        latency = GaugeMetricFamily(
            "locust_response_time_seconds",
            "Response time over the last few seconds, by quantile",
            labels=[*labels, "quantile"],
        )
        for (name, method), entry in self.environment.stats.entries.items():
            requests.add_metric([name, method], entry.num_requests)
            failures.add_metric([name, method], entry.num_failures)
            for q in QUANTILES:
                ms = entry.get_current_response_time_percentile(q)
                if ms is not None:
                    latency.add_metric([name, method, str(q)], ms / 1000)
        yield requests
        yield failures
        yield latency
        runner = self.environment.runner
        users = GaugeMetricFamily("locust_users", "Simulated clients running")
        users.add_metric([], runner.user_count if runner else 0)
        yield users


def serve_stats(environment: Environment, port: int) -> None:
    REGISTRY.register(LocustCollector(environment))
    start_http_server(port)
