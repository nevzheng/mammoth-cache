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

    CONFIG = "config"  # reads mammoth.toml
    AUTH = "auth"  # Basic and Bearer auth (doc 0002)
    DISK = "disk"  # L2 wired into the binary
    BUCKET = "bucket"  # L3 wired into the binary
    SIZE_LIMIT = "size-limit"  # a configurable entry limit small enough to test


class Bucket(BaseModel):
    model_config = ConfigDict(frozen=True)

    name: str
    prefix: str = ""
    endpoint: HttpUrl | None = None
    region: str = "us-east-1"
    access_key_id: SecretStr | None = None
    secret_access_key: SecretStr | None = None


class Credentials(BaseModel):
    model_config = ConfigDict(frozen=True)

    username: str
    password: SecretStr


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
    extra_base_urls: tuple[HttpUrl, ...] = ()
    prometheus_url: HttpUrl | None = None
    features: frozenset[Feature] = frozenset()
    max_entry_size: int = 100 * 1024 * 1024  # [limits] max_entry_size; 100 MiB by default
    control: bool = False
    destructive: bool = False
    bucket: Bucket | None = None
    users: dict[str, Credentials] = Field(default_factory=dict)
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

    def url(self, path: str, *, node: int = 0) -> str:
        base = str(self.base_url if node == 0 else self.extra_base_urls[node - 1])
        return base.rstrip("/") + "/" + path.lstrip("/")

    def has(self, *features: Feature) -> bool:
        return all(f in self.features for f in features)
