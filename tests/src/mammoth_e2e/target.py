"""Target profiles: where the system under test is and what the suites may do to it."""

from __future__ import annotations

import tomllib
from enum import StrEnum
from pathlib import Path
from typing import Self, override

from pydantic import BaseModel, ConfigDict, Field, HttpUrl, SecretStr
from pydantic_settings import BaseSettings, PydanticBaseSettingsSource, SettingsConfigDict


class Feature(StrEnum):
    """Server features a target may or may not have. Tests that need one skip without it."""

    CONFIG = "config"  # reads mammoth.toml, so limits and users are the profile's
    AUTH = "auth"  # enforces users, tokens and scopes (doc 0002)
    DISK = "disk"  # stores reach the disk tier and survive a restart
    BUCKET = "bucket"  # stores reach the bucket, and nodes refill from it
    BAZEL = "bazel"  # serves Bazel's HTTP cache on /bazel/ (doc 0012)


class Bucket(BaseModel):
    model_config = ConfigDict(frozen=True)

    name: str
    prefix: str = ""
    endpoint: HttpUrl | None = None
    region: str = "us-east-1"
    access_key_id: SecretStr | None = None
    secret_access_key: SecretStr | None = None


class Credentials(BaseModel):
    """A user and its token, sent as HTTP Basic (token as password) or as a Bearer token."""

    model_config = ConfigDict(frozen=True)

    username: str
    token: SecretStr
    scopes: frozenset[str] = frozenset()  # what the node grants this user (doc 0002)


class Compose(BaseModel):
    """How to drive the Docker Compose stack. Absent for targets the harness does not own."""

    model_config = ConfigDict(frozen=True)

    file: Path
    project: str = "mammoth"
    profiles: tuple[str, ...] = ()


class Target(BaseSettings):
    """A target profile, loaded from TOML. `MAMMOTH_E2E_*` environment variables override it."""

    model_config = SettingsConfigDict(
        env_prefix="MAMMOTH_E2E_",
        env_nested_delimiter="__",
        frozen=True,
        extra="forbid",
    )

    name: str
    base_url: HttpUrl
    # A node with the same users whose anonymous requests may read (`auth.anonymous`).
    open_base_url: HttpUrl | None = None
    extra_base_urls: tuple[HttpUrl, ...] = ()
    # A reverse proxy in front of a node, set up as docs/user/deployment.md says, and the same
    # proxy on its defaults. The node behind them takes entries of at least 100 MiB.
    proxy_url: HttpUrl | None = None
    proxy_defaults_url: HttpUrl | None = None
    prometheus_url: HttpUrl | None = None
    features: frozenset[Feature] = frozenset()
    max_entry_size: int = 100 * 1024 * 1024  # [limits] max_entry_size
    memory_max_entry_size: int = 1024 * 1024  # [memory] max_entry_size
    control: bool = False
    destructive: bool = False
    bucket: Bucket | None = None
    users: dict[str, Credentials] = Field(default_factory=dict)
    default_user: str | None = None  # sent by clients unless a test says otherwise
    anonymous_scopes: frozenset[str] = frozenset()  # [auth] anonymous on the node
    compose: Compose | None = None

    @classmethod
    @override
    def settings_customise_sources(
        cls,
        settings_cls: type[BaseSettings],
        init_settings: PydanticBaseSettingsSource,
        env_settings: PydanticBaseSettingsSource,
        dotenv_settings: PydanticBaseSettingsSource,
        file_secret_settings: PydanticBaseSettingsSource,
    ) -> tuple[PydanticBaseSettingsSource, ...]:
        # Environment variables win over the profile file, so CI can adjust one value.
        return env_settings, init_settings

    @classmethod
    def load(cls, path: Path) -> Self:
        with path.open("rb") as f:
            data = tomllib.load(f)
        if "compose" in data and "file" in data["compose"]:
            # Relative compose paths are relative to the profile, not the working directory.
            data["compose"]["file"] = str((path.parent / data["compose"]["file"]).resolve())
        return cls(**data)

    def base_urls(self) -> list[str]:
        """Every mammoth node of the target."""
        nodes = [self.base_url, *self.extra_base_urls]
        if self.open_base_url is not None:
            nodes.append(self.open_base_url)
        return [str(n).rstrip("/") for n in nodes]

    def url(self, path: str, *, base: HttpUrl | str | None = None) -> str:
        root = str(base if base is not None else self.base_url).rstrip("/")
        return root + "/" + path.lstrip("/")

    def has(self, *features: Feature) -> bool:
        return all(f in self.features for f in features)

    def user(self, name: str) -> Credentials:
        return self.users[name]

    @property
    def credentials(self) -> Credentials | None:
        """The default user's credentials, if the profile names one."""
        return self.users[self.default_user] if self.default_user else None
