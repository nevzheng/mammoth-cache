"""Load and traffic simulation for mammoth (doc 0011), with Locust.

Simulated build clients ask for entries the way CI fleets do: mostly hits on popular keys, a miss
and a store when inputs change. Each client is paced, so the run holds a request rate rather than
racing the server. The run's shape (`--run`) and pass/fail thresholds come from options; the
target comes from the same profile the pytest suites read.

    uv run --group load locust -f load/locustfile.py --headless --target targets/compose.toml \
        --run smoke

Client-side numbers are served on `--metrics-port` for Prometheus, next to mammoth's own.
"""

from __future__ import annotations

import hashlib
import math
from pathlib import Path
from typing import TYPE_CHECKING, ClassVar, override

from locust import LoadTestShape, constant_throughput, events, task
from locust.contrib.fasthttp import FastHttpUser
from locust.runners import WorkerRunner

from mammoth_e2e.client import basic
from mammoth_e2e.loadstats import serve_stats
from mammoth_e2e.stack import Stack
from mammoth_e2e.target import Target
from mammoth_e2e.traffic import Model, Traffic

if TYPE_CHECKING:
    from locust.argument_parser import LocustArgumentParser
    from locust.env import Environment

DEFAULT_MODEL = Model()
PER_USER_RATE = 5.0  # lookups a second per simulated client
GRADLE_CONTENT_TYPE = "application/vnd.gradle.build-cache-artifact.v2"

RUNS = {
    # name: (default rate, default duration in seconds)
    "smoke": (50.0, 30.0),
    "steady": (500.0, 600.0),
    "ramp": (2000.0, 600.0),
    "spike": (1000.0, 300.0),
}


@events.init_command_line_parser.add_listener
def _options(parser: LocustArgumentParser) -> None:
    group = parser.add_argument_group("mammoth")
    group.add_argument("--target", default="targets/compose.toml", help="Target profile (TOML)")
    group.add_argument("--up", action="store_true", help="Start the target's Compose stack first")
    group.add_argument("--run", choices=sorted(RUNS), default="smoke", help="Shape of the run")
    group.add_argument("--rate", type=float, default=0.0, help="Peak lookups a second")
    group.add_argument("--duration", type=float, default=0.0, help="Seconds")
    group.add_argument("--mix", default="8:1:1", help="Weights for Gradle:Maven:Bazel clients")
    group.add_argument("--keys", type=int, default=DEFAULT_MODEL.keys, help="Distinct entries")
    group.add_argument("--churn", type=float, default=DEFAULT_MODEL.churn, help="Share of new keys")
    group.add_argument("--max-p99-ms", type=float, default=100.0, help="Fail above this p99")
    group.add_argument("--max-fail-ratio", type=float, default=0.001, help="Fail above this")
    group.add_argument("--metrics-port", type=int, default=9646, help="0 turns the exporter off")


@events.init.add_listener
def _init(environment: Environment, **_: object) -> None:
    options = environment.parsed_options
    if options is None:
        return
    target = Target.load(Path(options.target))
    if (
        options.up
        and target.compose is not None
        and not isinstance(environment.runner, WorkerRunner)
    ):
        Stack(target.compose, target).up()
    if not environment.host:
        environment.host = str(target.base_url).rstrip("/")
    CacheUser.headers = {"Authorization": basic(target.credentials)} if target.credentials else {}
    CacheUser.model = Model(keys=options.keys, churn=options.churn)
    weights = [int(w) for w in options.mix.split(":")]
    for cls, weight in zip((GradleUser, MavenUser, BazelUser), weights, strict=True):
        cls.weight = weight
    if options.metrics_port and not isinstance(environment.runner, WorkerRunner):
        serve_stats(environment, options.metrics_port)


@events.quitting.add_listener
def _thresholds(environment: Environment, **_: object) -> None:
    options = environment.parsed_options
    if options is None:
        return
    total = environment.stats.total
    p99 = total.get_response_time_percentile(0.99) or 0.0
    failed = []
    if total.num_requests == 0:
        failed.append("no requests were sent")
    if p99 > options.max_p99_ms:
        failed.append(f"p99 {p99:.0f} ms > {options.max_p99_ms:.0f} ms")
    if total.fail_ratio > options.max_fail_ratio:
        failed.append(f"failures {total.fail_ratio:.2%} > {options.max_fail_ratio:.2%}")
    if failed:
        print("thresholds failed: " + "; ".join(failed))  # noqa: T201 - the run's verdict
        environment.process_exit_code = 1
    else:
        print(f"thresholds passed: p99 {p99:.0f} ms, failures {total.fail_ratio:.2%}")  # noqa: T201


class CacheUser(FastHttpUser):
    """One simulated build client. Subclasses speak one protocol."""

    abstract = True
    wait_time = constant_throughput(PER_USER_RATE)
    headers: ClassVar[dict[str, str]] = {}
    model: ClassVar[Model] = Model()

    @override
    def on_start(self) -> None:
        self.traffic = Traffic(self.model)

    def lookup(self, path: str, name: str) -> bool:
        """GET an entry; a 404 is a normal miss, not a failure. Returns whether it hit."""
        with self.client.get(
            path, name=name, headers=self.headers, catch_response=True
        ) as response:
            if response.status_code == 404:
                response.success()
            return response.status_code == 200

    def store(
        self, path: str, body: bytes, name: str, headers: dict[str, str] | None = None
    ) -> None:
        # FastHttpSession sends bytes as they are; its `data` annotation leaves bytes out.
        self.client.put(
            path,
            data=body,  # pyrefly: ignore[bad-argument-type]
            name=name,
            headers=self.headers | (headers or {}),
        )


class GradleUser(CacheUser):
    @task
    def build_step(self) -> None:
        rank = self.traffic.rank()
        path = f"/cache/{self.traffic.gradle_key(rank)}"
        if not self.lookup(path, "gradle GET"):
            ct = {"Content-Type": GRADLE_CONTENT_TYPE}
            self.store(path, self.traffic.body(rank), "gradle PUT", ct)


class MavenUser(CacheUser):
    @task
    def build_step(self) -> None:
        rank = self.traffic.rank()
        path = f"/maven/{self.traffic.maven_path(rank)}"
        if not self.lookup(path, "maven GET"):
            self.store(path, self.traffic.body(rank), "maven PUT")


class BazelUser(CacheUser):
    @task
    def build_step(self) -> None:
        rank = self.traffic.rank()
        action = hashlib.sha256(f"{self.model.seed}/action/{rank}".encode()).hexdigest()
        if self.lookup(f"/bazel/ac/{action}", "bazel AC GET"):
            return
        output = self.traffic.body(rank)
        digest = hashlib.sha256(output).hexdigest()
        self.store(f"/bazel/cas/{digest}", output, "bazel CAS PUT")
        # The action result names the output by digest; the server stores it as opaque bytes.
        self.store(f"/bazel/ac/{action}", f"{digest}/{len(output)}".encode(), "bazel AC PUT")


class Run(LoadTestShape):
    """Turns `--run`, `--rate` and `--duration` into a client count over time."""

    @override
    def tick(self) -> tuple[int, float] | None:
        runner = self.runner
        options = runner.environment.parsed_options if runner else None
        if options is None:
            return None
        default_rate, default_duration = RUNS[options.run]
        peak = options.rate or default_rate
        duration = options.duration or default_duration
        t = self.get_run_time()
        if t >= duration:
            return None
        rate = _rate(options.run, t, peak, duration)
        users = max(1, math.ceil(rate / PER_USER_RATE))
        return users, float(max(users, 10))


def _rate(run: str, t: float, peak: float, duration: float) -> float:
    match run:
        case "ramp":
            return peak * t / duration
        case "spike":
            return peak if t % 60 >= 50 else peak / 4  # 10 s bursts every minute
        case _:
            return peak
