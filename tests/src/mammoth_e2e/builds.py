"""Runs real Gradle and Maven builds of the fixture projects against a mammoth node."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING

from mammoth_e2e import tools

if TYPE_CHECKING:
    from mammoth_e2e.target import Credentials

FIXTURES = Path(__file__).resolve().parents[2] / "acceptance" / "fixtures"

_GRADLE_TASK = re.compile(
    r"^> Task (?P<path>:\S+)(?: (?P<outcome>[A-Z][A-Z-]+))?\s*$", re.MULTILINE
)


@dataclass(frozen=True, slots=True)
class BuildResult:
    argv: tuple[str, ...]
    returncode: int
    output: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0

    def describe(self) -> str:
        return f"$ {' '.join(self.argv)}\nexit {self.returncode}\n{self.output}"


@dataclass(frozen=True, slots=True)
class GradleResult(BuildResult):
    def outcomes(self) -> dict[str, str]:
        """Task path to outcome (`FROM-CACHE`, `UP-TO-DATE`, ...); executed tasks map to ``."""
        return {m["path"]: m["outcome"] or "" for m in _GRADLE_TASK.finditer(self.output)}

    def from_cache(self) -> set[str]:
        return {path for path, outcome in self.outcomes().items() if outcome == "FROM-CACHE"}

    def executed(self) -> set[str]:
        return {path for path, outcome in self.outcomes().items() if outcome == ""}


def _salt(project: Path, source: Path) -> None:
    """Give the copy a unique input so its cache keys are new on any server."""
    source.parent.mkdir(parents=True, exist_ok=True)
    source.write_text(
        f"package dev.mammoth.fixture;\n\nfinal class Salt {{\n"
        f'    static final String VALUE = "{uuid.uuid4().hex}";\n\n'
        f"    private Salt() {{}}\n}}\n"
    )
    assert source.is_relative_to(project)


def _run(argv: list[str], cwd: Path, env: dict[str, str], timeout: float) -> tuple[int, str]:
    result = subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    return result.returncode, result.stdout + result.stderr


@dataclass(slots=True)
class GradleProject:
    """A copy of the Gradle fixture, built by one Gradle version."""

    root: Path
    version: str
    cache_url: str
    credentials: Credentials | None = None
    push: bool = True
    expect_continue: bool = False
    java_home: str | None = field(default_factory=lambda: os.environ.get("JAVA_HOME"))

    @classmethod
    def create(cls, workdir: Path, version: str, cache_url: str) -> GradleProject:
        root = workdir / "gradle"
        shutil.copytree(FIXTURES / "gradle", root)
        _salt(root, root / "lib/src/main/java/dev/mammoth/fixture/Salt.java")
        return cls(root=root, version=version, cache_url=cache_url)

    def touch_app(self) -> None:
        """Change one source file in `:app` only."""
        source = self.root / "app/src/main/java/dev/mammoth/fixture/app/App.java"
        source.write_text(source.read_text() + f"// {uuid.uuid4().hex}\n")

    def build(self, *tasks: str, timeout: float = 600.0) -> GradleResult:
        home = tools.gradle_home(self.version)
        argv = [
            str(home / "bin" / "gradle"),
            "--console=plain",
            "--no-daemon",
            "--build-cache",
            "--stacktrace",
            *(tasks or ("clean", "build")),
        ]
        env = os.environ.copy()
        env["GRADLE_USER_HOME"] = str(tools.cache_dir() / "gradle-user-home")
        env["MAMMOTH_CACHE_URL"] = self.cache_url
        env["MAMMOTH_CACHE_PUSH"] = str(self.push).lower()
        env["MAMMOTH_EXPECT_CONTINUE"] = str(self.expect_continue).lower()
        if self.credentials is not None:
            env["MAMMOTH_CACHE_USER"] = self.credentials.username
            env["MAMMOTH_CACHE_PASSWORD"] = self.credentials.password.get_secret_value()
        if self.java_home:
            env["JAVA_HOME"] = self.java_home
        code, output = _run(argv, self.root, env, timeout)
        return GradleResult(argv=tuple(argv), returncode=code, output=output)


_SETTINGS = """<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0">
  <servers>
    <server>
      <id>mammoth</id>
      <username>{username}</username>
      <password>{password}</password>
    </server>
  </servers>
</settings>
"""

_EXTENSIONS = """<extensions xmlns="http://maven.apache.org/EXTENSIONS/1.1.0">
  <extension>
    <groupId>org.apache.maven.extensions</groupId>
    <artifactId>maven-build-cache-extension</artifactId>
    <version>{version}</version>
  </extension>
</extensions>
"""


@dataclass(slots=True)
class MavenProject:
    """A copy of the Maven fixture, built by one Maven and extension version."""

    root: Path
    maven_version: str
    extension_version: str
    cache_url: str
    credentials: Credentials | None = None
    push: bool = True
    java_home: str | None = field(default_factory=lambda: os.environ.get("JAVA_HOME"))

    @classmethod
    def create(
        cls, workdir: Path, maven_version: str, extension_version: str, cache_url: str
    ) -> MavenProject:
        root = workdir / "maven"
        shutil.copytree(FIXTURES / "maven", root)
        (root / ".mvn").mkdir(exist_ok=True)
        (root / ".mvn" / "extensions.xml").write_text(_EXTENSIONS.format(version=extension_version))
        _salt(root, root / "lib/src/main/java/dev/mammoth/fixture/Salt.java")
        return cls(
            root=root,
            maven_version=maven_version,
            extension_version=extension_version,
            cache_url=cache_url,
        )

    def touch_app(self) -> None:
        source = self.root / "app/src/main/java/dev/mammoth/fixture/app/App.java"
        source.write_text(source.read_text() + f"// {uuid.uuid4().hex}\n")

    def build(self, *goals: str, timeout: float = 600.0) -> BuildResult:
        """Build with a fresh local build cache, so anything restored came from the server."""
        home = tools.maven_home(self.maven_version)
        local_cache = self.root / "target-build-cache" / uuid.uuid4().hex
        settings = self.root / "settings.xml"
        username, password = ("anonymous", "")
        if self.credentials is not None:
            username = self.credentials.username
            password = self.credentials.password.get_secret_value()
        settings.write_text(_SETTINGS.format(username=username, password=password))
        argv = [
            str(home / "bin" / "mvn"),
            "--batch-mode",
            "--no-transfer-progress",
            "--settings",
            str(settings),
            f"-Dmaven.repo.local={tools.cache_dir() / 'm2-repository'}",
            "-Dmaven.build.cache.enabled=true",
            f"-Dmaven.build.cache.location={local_cache}",
            "-Dmaven.build.cache.remote.enabled=true",
            f"-Dmaven.build.cache.remote.url={self.cache_url}",
            "-Dmaven.build.cache.remote.server.id=mammoth",
            f"-Dmaven.build.cache.remote.save.enabled={str(self.push).lower()}",
            *(goals or ("clean", "verify")),
        ]
        env = os.environ.copy()
        if self.java_home:
            env["JAVA_HOME"] = self.java_home
        code, output = _run(argv, self.root, env, timeout)
        return BuildResult(argv=tuple(argv), returncode=code, output=output)
