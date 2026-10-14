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

import dataclasses
import hashlib
import math
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any, ClassVar, override

from locust import LoadTestShape, constant_throughput, events, task
from locust.contrib.fasthttp import FastHttpUser
from locust.runners import WorkerRunner

from mammoth_e2e import metrics
from mammoth_e2e.client import basic
from mammoth_e2e.loadstats import serve_stats
from mammoth_e2e.stack import Stack
from mammoth_e2e.target import Target
from mammoth_e2e.tlscerts import Authority
from mammoth_e2e.traffic import MODELS, Model, Traffic

if TYPE_CHECKING:
    from locust.argument_parser import LocustArgumentParser
    from locust.env import Environment

PER_USER_RATE = 5.0  # lookups a second per simulated client
GRADLE_CONTENT_TYPE = "application/vnd.gradle.build-cache-artifact.v2"

RUNS = {
    # name: (default rate, default duration in seconds)
    "smoke": (50.0, 30.0),
    "steady": (500.0, 600.0),
    "ramp": (2000.0, 600.0),
    "spike": (1000.0, 300.0),
    # A warm incremental build: the model's entries are stored first, then read back at a high
    # rate over keep-alive connections, almost all hits.
    "warm": (2000.0, 120.0),
}
SEED = Path(__file__).with_name("seed.py")


@events.init_command_line_parser.add_listener
def _options(parser: LocustArgumentParser) -> None:
    group = parser.add_argument_group("mammoth")
    group.add_argument("--target", default="targets/compose.toml", help="Target profile (TOML)")
    group.add_argument("--up", action="store_true", help="Start the target's Compose stack first")
    group.add_argument("--run", choices=sorted(RUNS), default="smoke", help="Shape of the run")
    group.add_argument("--rate", type=float, default=0.0, help="Peak lookups a second")
    group.add_argument("--duration", type=float, default=0.0, help="Seconds")
    group.add_argument("--mix", default="8:1:1", help="Weights for Gradle:Maven:Bazel clients")
    group.add_argument("--model", choices=sorted(MODELS), help="Traffic model (default per run)")
    group.add_argument("--keys", type=int, default=0, help="Distinct entries (0: the model's)")
    group.add_argument("--churn", type=float, help="Share of new keys (default: the model's)")
    group.add_argument("--no-seed", action="store_true", help="Skip storing the warm entries")
    group.add_argument("--max-p99-ms", type=float, default=100.0, help="Fail above this p99")
    group.add_argument("--max-fail-ratio", type=float, default=0.001, help="Fail above this")
    group.add_argument("--metrics-port", type=int, default=9646, help="0 turns the exporter off")
    group.add_argument(
        "--tls", action="store_true", help="Load the profile's [tls] node over HTTPS instead"
    )


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
    _choose_host(environment, target, tls=options.tls)
    CacheUser.headers = {"Authorization": basic(target.credentials)} if target.credentials else {}
    model = MODELS[options.model or ("warm" if options.run == "warm" else "synthetic")]
    if options.keys:
        model = model.with_keys(options.keys)
    if options.churn is not None:
        model = dataclasses.replace(model, churn=options.churn)
    CacheUser.model = model
    mix = options.mix
    if options.run == "warm":
        mix = "1:0:0"  # the seeder stores Gradle entries
        if not options.no_seed and not isinstance(environment.runner, WorkerRunner):
            argv = [sys.executable, str(SEED), "--target", options.target]
            argv += ["--model", options.model or "warm", "--keys", str(model.keys)]
            subprocess.run(argv, check=True)
    _report["target"] = target
    weights = [int(w) for w in mix.split(":")]
    for cls, weight in zip((GradleUser, MavenUser, BazelUser), weights, strict=True):
        cls.weight = weight
    if options.metrics_port and not isinstance(environment.runner, WorkerRunner):
        serve_stats(environment, options.metrics_port)


_report: dict[str, Any] = {}


def _choose_host(environment: Environment, target: Target, *, tls: bool) -> None:
    """The profile's main node, or with `tls` its TLS node, unless `--host` names another."""
    if tls:
        _use_tls(environment, target)
    if not environment.host:
        environment.host = str(target.base_url).rstrip("/")


def _use_tls(environment: Environment, target: Target) -> None:
    """Load the profile's TLS node, verifying it against the harness's test CA as a real client
    would."""
    if target.tls is None:
        raise SystemExit(f"--tls: target {target.name!r} has no [tls] nodes")
    authority = Authority(target.tls.cert_dir)
    CacheUser.insecure = False
    CacheUser.ssl_context_factory = authority.context
    _report["base"] = str(target.tls.base_url).rstrip("/")
    _report["verify"] = authority.context()
    environment.host = environment.host or _report["base"]


@events.test_start.add_listener
def _before(environment: Environment, **_: object) -> None:
    if isinstance(environment.runner, WorkerRunner):
        return
    target: Target | None = _report.get("target")
    if target is not None:
        _report["before"] = _scrape(target)


def _scrape(target: Target) -> metrics.Scrape:
    """The loaded node's metrics: the TLS node's with --tls."""
    return metrics.scrape(target, base=_report.get("base"), verify=_report.get("verify", True))


def _tier_hits(target: Target, before: metrics.Scrape) -> str:
    """Where hits were served from, by the node's own counters."""
    after = _scrape(target)
    by_tier = {
        tier: metrics.delta(before, after, "mammoth_cache_lookups_total", result="hit", tier=tier)
        for tier in ("memory", "disk", "bucket")
    }
    if any(by_tier.values()):
        return ", ".join(f"{tier} {n:.0f}" for tier, n in by_tier.items())
    # Older nodes only count promotions; every disk hit is promoted to memory.
    promoted = metrics.delta(before, after, "mammoth_tier_promotions_total", **{"from": "disk"})
    return f"disk {promoted:.0f} (from promotions), the rest from memory"


@events.quitting.add_listener
def _thresholds(environment: Environment, **_: object) -> None:
    options = environment.parsed_options
    if options is None or isinstance(environment.runner, WorkerRunner):
        return  # workers hold no totals; the master reports
    total = environment.stats.total
    p50 = total.get_response_time_percentile(0.5) or 0.0
    p99 = total.get_response_time_percentile(0.99) or 0.0
    print(  # noqa: T201 - the run's numbers
        f"{options.run}: {total.num_requests} requests, {total.total_rps:.0f}/s, "
        f"p50 {p50:.0f} ms, p99 {p99:.0f} ms"
    )
    target: Target | None = _report.get("target")
    before: metrics.Scrape | None = _report.get("before")
    if target is not None and before is not None:
        print(f"hits by tier: {_tier_hits(target, before)}")  # noqa: T201
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
