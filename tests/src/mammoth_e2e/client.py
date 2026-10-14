"""HTTP clients for the two cache protocols, as Gradle and the Maven extension speak them."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING

import httpx

if TYPE_CHECKING:
    from collections.abc import Iterator

    from mammoth_e2e.target import Credentials, Target


def _auth(credentials: Credentials | None) -> httpx.BasicAuth | None:
    if credentials is None:
        return None
    return httpx.BasicAuth(credentials.username, credentials.password.get_secret_value())


@dataclass(slots=True)
class CacheClient:
    """Talks to `/cache/{key}` and `/maven/{path}` on one node."""

    target: Target
    node: int = 0
    credentials: Credentials | None = None
    user_agent: str = "mammoth-e2e"
    _http: httpx.Client = field(init=False)

    def __post_init__(self) -> None:
        self._http = httpx.Client(
            base_url=self.target.url("/", node=self.node),
            auth=_auth(self.credentials),
            headers={"User-Agent": self.user_agent},
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

    def gradle_put(self, key: str, body: bytes) -> httpx.Response:
        return self.put(
            f"/cache/{key}",
            body,
            headers={"Content-Type": "application/vnd.gradle.build-cache-artifact.v2"},
        )

    def maven_get(self, path: str) -> httpx.Response:
        return self.get(f"/maven/{path}")

    def maven_put(self, path: str, body: bytes) -> httpx.Response:
        return self.put(f"/maven/{path}", body)
