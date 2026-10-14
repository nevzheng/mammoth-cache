"""Run pytest or Locust in the source tree's tests/: the Compose-backed suite targets.

These suites drive Docker, Java and the network on the host, so they run where `uv run` would:
in the real tests/ directory, with the Python and packages Bazel provides.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

# The source tree's tests/: runfiles are symlinks into it, and the suites run outside the sandbox.
TESTS = Path(__file__).resolve().parent.parent


def pytest_main(args: list[str]) -> int:
    import pytest  # noqa: PLC0415  (only this entry point needs it)

    options = ["-p", "no:cacheprovider"]
    # Bazel's test.xml becomes pytest's JUnit report, with a result per test case.
    if xml := os.environ.get("XML_OUTPUT_FILE"):
        options.append(f"--junitxml={xml}")
    return int(pytest.main([*options, *args]))


def locust_main(args: list[str]) -> int:
    from locust.main import main as locust  # noqa: PLC0415  (gevent patches on import)

    sys.argv = ["locust", *args]
    locust()
    return 0


def main() -> int:
    runner, *args = sys.argv[1:]
    os.chdir(TESTS)
    return {"pytest": pytest_main, "locust": locust_main}[runner](args)


if __name__ == "__main__":
    sys.exit(main())
