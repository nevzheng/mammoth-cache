"""Run ruff or pyrefly over the suites' sources: the Bazel lint tests in tests/BUILD.bazel."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

# The copy of tests/ in the test's runfiles, not the source tree: Bazel tracks these inputs.
TESTS = Path(__file__).absolute().parent.parent

CHECKS = {
    "ruff-format": ("ruff", ["format", "--check"]),
    "ruff-check": ("ruff", ["check"]),
    "pyrefly": ("pyrefly", ["check", "--config", "pyproject.toml"]),
}


def pyrefly_paths() -> list[str]:
    """Point pyrefly at this interpreter's packages instead of letting it look for a Python."""
    args = ["--skip-interpreter-query", "--python-version", "3.14", "--search-path", "src"]
    for entry in sys.path:
        if entry and Path(entry).is_dir() and not Path(entry).is_relative_to(TESTS):
            args += ["--site-package-path", entry]
    return args


def main() -> int:
    # <check> <the tool wheel's data files> -- <the Python files, relative to the runfiles root>
    check, *rest = sys.argv[1:]
    split = rest.index("--")
    tools, files = rest[:split], rest[split + 1 :]
    name, args = CHECKS[check]
    # The wheel's executable is <repository>/bin/<name>.
    binary = next(Path(tool).absolute() for tool in tools if Path(tool).name == name)
    if name == "pyrefly":
        args = args + pyrefly_paths()
    # Paths relative to tests/, so the per-file settings in pyproject.toml apply.
    sources = [str(Path(file).relative_to("tests")) for file in files]
    os.chdir(TESTS)
    return subprocess.run([str(binary), *args, *sources], check=False).returncode


if __name__ == "__main__":
    sys.exit(main())
