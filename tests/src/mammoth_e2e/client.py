"""HTTP clients for the cache protocols, as Gradle, Maven and Bazel speak them."""

from __future__ import annotations

import base64
import hashlib
from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING

import httpx

if TYPE_CHECKING:
    from collections.abc import Iterator

    from mammoth_e2e.target import Credentials, Target


class Auth(Enum):
    """How a client presents its credentials."""

    BASIC = "basic"
    BEARER = "bearer"


def basic(credentials: Credentials) -> str:
    pair = f"{credentials.username}:{credentials.token.get_secret_value()}"
    return "Basic " + base64.b64encode(pair.encode()).decode()


def bearer(credentials: Credentials) -> str:
    return f"Bearer {credentials.token.get_secret_value()}"


def auth_headers(credentials: Credentials | None) -> dict[str, str]:
    return {"Authorization": basic(credentials)} if credentials is not None else {}


def sha256(body: bytes) -> str:
    return hashlib.sha256(body).hexdigest()


@dataclass(slots=True)
class CacheClient:
    """Talks to `/cache/`, `/maven/` and `/bazel/` on one node.

    `credentials=None` sends no `Authorization` header; `headers` can set any header, including a
    malformed `Authorization`.
    """

    target: Target
    credentials: Credentials | None = None
    auth: Auth = Auth.BASIC
    base_url: str | None = None
    user_agent: str = "mammoth-e2e"
    headers: dict[str, str] = field(default_factory=dict)
    _http: httpx.Client = field(init=False)

    def __post_init__(self) -> None:
        headers = {"User-Agent": self.user_agent}
        if self.credentials is not None:
            render = basic if self.auth is Auth.BASIC else bearer
            headers["Authorization"] = render(self.credentials)
        headers.update(self.headers)
        self._http = httpx.Client(
            base_url=self.base_url or self.target.url("/"),
            headers=headers,
            timeout=httpx.Timeout(30.0),
        )

    def close(self) -> None:
        self._http.close()

    def get(self, path: str) -> httpx.Response:
        return self._http.get(path)

    def head(self, path: str) -> httpx.Response:
        return self._http.head(path)

    def put(
        self, path: str, body: bytes | Iterator[bytes], headers: dict[str, str] | None = None
    ) -> httpx.Response:
        return self._http.put(path, content=body, headers=headers)

    def request(self, method: str, path: str) -> httpx.Response:
        return self._http.request(method, path)

    def gradle_get(self, key: str) -> httpx.Response:
        return self.get(f"/cache/{key}")

    def gradle_put(self, key: str, body: bytes | Iterator[bytes]) -> httpx.Response:
        content_type = {"Content-Type": "application/vnd.gradle.build-cache-artifact.v2"}
        return self.put(f"/cache/{key}", body, headers=content_type)

    def maven_get(self, path: str) -> httpx.Response:
        return self.get(f"/maven/{path}")

    def maven_put(self, path: str, body: bytes) -> httpx.Response:
        return self.put(f"/maven/{path}", body)

    def bazel_get(self, store: str, digest: str) -> httpx.Response:
        return self.get(f"/bazel/{store}/{digest}")

    def bazel_put(self, store: str, digest: str, body: bytes) -> httpx.Response:
        return self.put(f"/bazel/{store}/{digest}", body)
