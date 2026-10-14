"""Lifecycle of the Docker Compose stack in deploy/compose."""

from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

import httpx

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

    def _run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        argv = ["docker", "compose", "-f", str(self.compose.file), "-p", self.compose.project]
        for profile in self.compose.profiles:
            argv += ["--profile", profile]
        result = subprocess.run(
            [*argv, *args], capture_output=True, text=True, check=False, env=os.environ.copy()
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

    def restart(self, *services: str) -> None:
        self._run("restart", *services)
        self.wait_ready()

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
        for base in self.target.base_urls():
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
