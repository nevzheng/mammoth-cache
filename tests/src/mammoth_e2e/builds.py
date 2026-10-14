"""Runs real Gradle and Maven builds of the fixture projects against a mammoth node."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Literal

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
            env["MAMMOTH_CACHE_PASSWORD"] = self.credentials.token.get_secret_value()
        if self.java_home:
            env["JAVA_HOME"] = self.java_home
        code, output = _run(argv, self.root, env, timeout)
        return GradleResult(argv=tuple(argv), returncode=code, output=output)


_SETTINGS = """<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0">
  <servers>{servers}</servers>
</settings>
"""

_SERVER = """
    <server>
      <id>mammoth</id>
      <username>{username}</username>
      <password>{password}</password>
    </server>
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
        servers = ""
        if self.credentials is not None:
            servers = _SERVER.format(
                username=self.credentials.username,
                password=self.credentials.token.get_secret_value(),
            )
        settings.write_text(_SETTINGS.format(servers=servers))
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


type Download = Literal["all", "toplevel", "minimal"]

_PROCESSES = re.compile(r"INFO: \d+ process(?:es)?: (?P<kinds>[^.]*)\.")


@dataclass(frozen=True, slots=True)
class BazelResult(BuildResult):
    def processes(self) -> dict[str, int]:
        """The build's process summary, e.g. `{"remote cache hit": 2, "internal": 1}`."""
        summaries = list(_PROCESSES.finditer(self.output))
        if not summaries:
            return {}
        match = summaries[-1]  # the last summary is the build's
        kinds: dict[str, int] = {}
        for part in match["kinds"].split(","):
            count, _, kind = part.strip().partition(" ")
            kinds[kind] = int(count)
        return kinds

    def remote_hits(self) -> int:
        return self.processes().get("remote cache hit", 0)

    def executed(self) -> int:
        return sum(
            n for k, n in self.processes().items() if k not in {"internal", "remote cache hit"}
        )


@dataclass(slots=True)
class BazelProject:
    """A copy of the Bazel fixture (genrules only), built by one Bazel version.

    Every build uses a fresh output base, so local caches are empty and any hit is remote.
    """

    root: Path
    version: str
    cache_url: str
    credentials: Credentials | None = None
    auth_in_header: bool = False  # Bearer via --remote_header instead of user:token in the URL
    upload: bool = True
    download: Download = "toplevel"  # Bazel's default since 7

    @classmethod
    def create(cls, workdir: Path, version: str, cache_url: str) -> BazelProject:
        root = workdir / "bazel"
        shutil.copytree(FIXTURES / "bazel", root)
        (root / "lib" / "salt.txt").write_text(uuid.uuid4().hex + "\n")
        return cls(root=root, version=version, cache_url=cache_url)

    @property
    def grpc(self) -> bool:
        return self.cache_url.startswith(("grpc://", "grpcs://"))

    def touch_app(self) -> None:
        source = self.root / "app" / "app.txt"
        source.write_text(source.read_text() + uuid.uuid4().hex + "\n")

    def add_blob(self, name: str, size: int, deps: tuple[str, ...] = ()) -> str:
        """Adds `//gen:{name}`, a genrule writing `size` bytes derived from the salt and `deps`.

        Returns its label. The bytes are deterministic, so a rebuild that executes it again
        writes the same output, and they are unique to this copy of the project.
        """
        package = self.root / "gen"
        package.mkdir(exist_ok=True)
        build = package / "BUILD.bazel"
        srcs = ", ".join(f'"{d}"' for d in ("//lib:salt.txt", *deps))
        locations = " ".join(f"$(execpaths {d})" for d in ("//lib:salt.txt", *deps))
        # `yes` repeats the inputs' digest, so any size from 0 up is one `head -c`. Genrules run
        # with pipefail and `yes` dies of SIGPIPE, hence the `|| true`.
        build.write_text(
            (build.read_text() if build.exists() else "") + f"genrule(\n"
            f'    name = "{name}",\n'
            f"    srcs = [{srcs}],\n"
            f'    outs = ["{name}.out"],\n'
            f'    cmd = "(yes \\"$$(cat {locations} | sha256sum)\\" || true)'
            f' | head -c {size} > $@",\n'
            f'    visibility = ["//visibility:public"],\n'
            f")\n"
        )
        lib = self.root / "lib" / "BUILD.bazel"
        if "exports_files" not in lib.read_text():
            lib.write_text(lib.read_text() + 'exports_files(["salt.txt"])\n')
        return f"//gen:{name}"

    def outputs(self) -> dict[str, bytes]:
        """Every file under `bazel-bin`, by path relative to it, from the last build."""
        bin_dir = self.root / "bazel-bin"
        return {
            str(p.relative_to(bin_dir)): p.read_bytes()
            for p in sorted(bin_dir.rglob("*.out"))
            if p.is_file()
        }

    def remote_cache_flags(self) -> list[str]:
        url = self.cache_url
        flags: list[str] = []
        if self.credentials is not None:
            token = self.credentials.token.get_secret_value()
            # A grpc:// URL has no place for credentials; Bazel only sends headers there.
            if self.auth_in_header or self.grpc:
                flags.append(f"--remote_header=Authorization=Bearer {token}")
            else:
                scheme, _, rest = url.partition("://")
                url = f"{scheme}://{self.credentials.username}:{token}@{rest}"
        flags += [
            f"--remote_cache={url}",
            f"--remote_upload_local_results={str(self.upload).lower()}",
            f"--remote_download_{self.download}",
        ]
        return flags

    def build(self, *targets: str, timeout: float = 600.0) -> BazelResult:
        binary = tools.bazel_binary(self.version)
        output_base = self.root.parent / f"bazel-out-{uuid.uuid4().hex[:8]}"
        cache = tools.cache_dir()
        argv = [
            str(binary),
            "--ignore_all_rc_files",
            f"--output_user_root={cache / 'bazel-user-root'}",
            f"--output_base={output_base}",
            "--max_idle_secs=5",
            *tools.bazel_startup_args(),
            "build",
            f"--repository_cache={cache / 'bazel-repository-cache'}",
            "--lockfile_mode=off",
            "--noshow_progress",
            "--color=no",
            "--curses=no",
            "--spawn_strategy=local",
            *[f"--registry={r}" for r in tools.bazel_registries()],
            *self.remote_cache_flags(),
            *(targets or ("//...",)),
        ]
        env = os.environ.copy()
        env["HOME"] = str(cache / "bazel-home")  # no ~/.bazelrc or credential helpers
        code, output = _run(argv, self.root, env, timeout)
        return BazelResult(argv=tuple(argv), returncode=code, output=output)


# Buck2 prefixes each line of its summary with a timestamp in brackets.
_BUCK2_COMMANDS = re.compile(
    r"^(?:\[[^\]]*\] )?Commands: (?P<total>\d+) "
    r"\(cached: (?P<cached>\d+), remote: (?P<remote>\d+), local: (?P<local>\d+)\)",
    re.MULTILINE,
)


@dataclass(frozen=True, slots=True)
class Buck2Result(BuildResult):
    def commands(self) -> dict[str, int]:
        """The build's command summary: `total`, `cached`, `remote` and `local`."""
        found = list(_BUCK2_COMMANDS.finditer(self.output))
        return {k: int(v) for k, v in found[-1].groupdict().items()} if found else {}

    def remote_hits(self) -> int:
        return self.commands().get("cached", 0)

    def executed(self) -> int:
        return self.commands().get("local", 0)


@dataclass(slots=True)
class Buck2Project:
    """A copy of the Buck2 fixture (its own rules, no prelude), built by one Buck2 release.

    Every build ends with `buck2 clean`, which stops the daemon and empties `buck-out`, so the
    next build's cached actions came from the remote cache. (A fresh isolation directory would
    not do: it changes the scratch path in each action's environment, and so its digest.)
    """

    root: Path
    version: str
    address: str  # grpc://host:port
    credentials: Credentials | None = None
    upload: bool = True

    @classmethod
    def create(cls, workdir: Path, version: str, address: str) -> Buck2Project:
        root = workdir / "buck2"
        shutil.copytree(FIXTURES / "buck2", root)
        (root / "salt.txt").write_text(uuid.uuid4().hex + "\n")
        return cls(root=root, version=version, address=address)

    def touch_app(self) -> None:
        source = self.root / "app.txt"
        source.write_text(source.read_text() + uuid.uuid4().hex + "\n")

    def add_blob(self, name: str, size: int) -> str:
        """Adds `//:{name}`, writing `size` bytes derived from the salt. Returns its label."""
        build = self.root / "BUCK"
        build.write_text(
            build.read_text() + f'\nblob(name = "{name}", size = {size}, srcs = ["salt.txt"])\n'
        )
        return f"//:{name}"

    def _config(self) -> str:
        lines = [
            "[buck2_re_client]",
            *(f"{k}_address = {self.address}" for k in ("engine", "action_cache", "cas")),
            "tls = false",
        ]
        if self.credentials is not None:
            # Expanded from the daemon's environment, so the token is never in a file.
            lines.append("http_headers = Authorization: Bearer $MAMMOTH_E2E_BUCK2_TOKEN")
        lines += ["", "[mammoth]", f"upload = {str(self.upload).lower()}", ""]
        return "\n".join(lines)

    def build(self, *targets: str, timeout: float = 600.0) -> Buck2Result:
        binary = tools.buck2_binary(self.version)
        (self.root / ".buckconfig.local").write_text(self._config())
        env = os.environ.copy()
        home = tools.cache_dir() / "buck2-home"  # no ~/.buckconfig; the daemons' state
        home.mkdir(exist_ok=True)
        env["HOME"] = str(home)
        if self.credentials is not None:
            env["MAMMOTH_E2E_BUCK2_TOKEN"] = self.credentials.token.get_secret_value()
        argv = [
            str(binary),
            "build",
            "--console=simple",
            "--no-interactive-console",
            *(targets or ("//:lib", "//:app")),
        ]
        try:
            code, output = _run(argv, self.root, env, timeout)
        finally:
            _run([str(binary), "clean"], self.root, env, 120.0)
        return Buck2Result(argv=tuple(argv), returncode=code, output=output)
