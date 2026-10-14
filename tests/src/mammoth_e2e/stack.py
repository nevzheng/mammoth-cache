"""Lifecycle of the Docker Compose stack in deploy/compose."""

from __future__ import annotations

import json
import os
import subprocess
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

import httpx

from mammoth_e2e.bucket import BucketInspector

if TYPE_CHECKING:
    from collections.abc import Sequence

    from mammoth_e2e.target import Compose, Target


class StackError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class Stack:
    """Starts, stops and inspects the services of one Compose project."""

    compose: Compose
    target: Target

    def _run(
        self, *args: str, check: bool = True, timeout: float | None = None
    ) -> subprocess.CompletedProcess[str]:
        argv = ["docker", "compose", "-f", str(self.compose.file), "-p", self.compose.project]
        for profile in self.compose.profiles:
            argv += ["--profile", profile]
        result = subprocess.run(
            [*argv, *args],
            capture_output=True,
            text=True,
            check=False,
            env=os.environ.copy(),
            timeout=timeout,
        )
        if check and result.returncode != 0:
            msg = f"docker compose {' '.join(args)} failed:\n{result.stdout}\n{result.stderr}"
            raise StackError(msg)
        return result

    def up(self) -> None:
        # --no-build: the image under test is built (or pulled) before the suite runs.
        self._run("up", "--detach", "--wait", "--no-build", "--remove-orphans")
        self.wait_ready()

    def down(self) -> None:
        self._run("down", "--volumes", "--remove-orphans", check=False)

    def stop(self, *services: str) -> None:
        self._run("stop", *services)

    def start(self, *services: str) -> None:
        self._run("start", *services)
        self.wait_ready()
        if "seaweedfs" in services:
            self.wait_bucket()

    def restart(self, *services: str) -> None:
        self._run("restart", *services)
        self.wait_ready()

    def pause(self, *services: str) -> None:
        """Freeze services: connections open but nothing answers, like a network blackhole."""
        self._run("pause", *services)

    def unpause(self, *services: str) -> None:
        self._run("unpause", *services)
        if "seaweedfs" in services:
            self.wait_bucket()

    def wait_bucket(self, timeout: float = 60.0) -> None:
        """Wait until the bucket takes writes again: SeaweedFS answers health checks before its
        master has a leader, and writes fail until it does."""
        if self.target.bucket is None:
            return
        bucket = BucketInspector.connect(self.target.bucket)
        deadline = time.monotonic() + timeout
        while not bucket.writable():
            if time.monotonic() > deadline:
                raise StackError(f"bucket not writable after {timeout}s")
            time.sleep(0.5)

    def kill(self, service: str, signal: str = "SIGTERM") -> None:
        """Send a signal to a service's container, as Kubernetes does when it stops a pod."""
        self._run("kill", "--signal", signal, service)

    def wait_exited(self, service: str, timeout: float = 60.0) -> int:
        """Wait for a service's container to exit and return its exit code."""
        deadline = time.monotonic() + timeout
        while True:
            out = self._run("ps", "--all", "--format", "json", service).stdout
            for line in out.splitlines():
                container = json.loads(line)
                if container["State"] == "exited":
                    return int(container["ExitCode"])
            if time.monotonic() > deadline:
                raise StackError(f"{service} still running after {timeout}s")
            time.sleep(0.2)

    def run_once(self, service: str, timeout: float = 30.0) -> tuple[int | None, str]:
        """Start a throwaway second container of a service, sharing its volumes.

        Returns its exit code and output; the code is None if it was still running at `timeout`.
        """
        try:
            result = self._run("run", "--rm", "--no-deps", "-T", service, check=False,
                               timeout=timeout)  # fmt: skip
        except subprocess.TimeoutExpired as e:
            project = f"label=com.docker.compose.project={self.compose.project}"
            ids = subprocess.run(
                ["docker", "ps", "-q", "--filter", project, "--filter",
                 "label=com.docker.compose.oneoff=True"],
                capture_output=True, text=True, check=False,
            ).stdout.split()  # fmt: skip
            if ids:
                subprocess.run(["docker", "rm", "-f", *ids], capture_output=True, check=False)
            output = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
            return None, output
        return result.returncode, result.stdout + result.stderr

    def wipe_disk(self, service: str) -> None:
        """Empty a stopped node's disk volume, as if it were a new node."""
        self._run(
            "run", "--rm", "--no-deps", "--entrypoint", "/bin/sh", "init",
            "-c", f"rm -rf /disks/{service}/* && chown 65532:65532 /disks/{service}",
        )  # fmt: skip

    def logs(self, services: Sequence[str] = (), *, tail: int = 200) -> str:
        args = ("logs", "--no-color", "--timestamps", f"--tail={tail}", *services)
        return self._run(*args, check=False).stdout

    def wait_ready(self, timeout: float = 60.0) -> None:
        """Wait until every mammoth node answers /readyz with 200."""
        deadline = time.monotonic() + timeout
        proxies = [self.target.proxy_url, self.target.proxy_defaults_url]
        proxies += self.target.topologies.values()
        for base in self.target.base_urls() + [str(p).rstrip("/") for p in proxies if p]:
            url = f"{base}/readyz"
            while True:
                try:
                    if httpx.get(url, timeout=2.0).status_code == 200:
                        break
                except httpx.TransportError:
                    pass
                if time.monotonic() > deadline:
                    raise StackError(f"{url} not ready after {timeout}s")
                time.sleep(0.2)
