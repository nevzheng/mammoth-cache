"""The cache protocols as data: routes, key shapes and store semantics, for the property suites."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass

from mammoth_e2e.target import Feature


@dataclass(frozen=True, slots=True)
class Protocol:
    name: str
    prefix: str  # the route without the key, e.g. "/cache/"
    template: str  # the path template in the OpenAPI contract
    metrics_route: str  # the `route` label on mammoth_http_requests_total
    mutable: bool  # a store to an existing key replaces it
    content_addressed: bool  # the key is the SHA-256 of the body
    feature: Feature | None = None  # the server feature that turns it on

    def path(self, name: str) -> str:
        return self.prefix + name

    def accepts(self, name: str) -> bool:
        """Whether `name` is a valid key for this protocol (docs 0001, 0012)."""
        hexadecimal = all(c in "0123456789abcdef" for c in name)
        match self.name:
            case "gradle":
                return 1 <= len(name) <= 128 and hexadecimal
            case "maven":
                segments = name.split("/")
                return (
                    len(name) <= 512
                    and len(segments) <= 16
                    and all(_maven_segment(s) for s in segments)
                )
            case _:
                return len(name) == 64 and hexadecimal


def _maven_segment(segment: str) -> bool:
    allowed = set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-")
    return segment not in {"", ".", ".."} and set(segment) <= allowed


GRADLE = Protocol(
    "gradle", "/cache/", "/cache/{key}", "/cache/{key}", mutable=False, content_addressed=False
)
MAVEN = Protocol(
    "maven", "/maven/", "/maven/{path}", "/maven/{*path}", mutable=True, content_addressed=False
)
BAZEL_AC = Protocol(
    "bazel-ac",
    "/bazel/ac/",
    "/bazel/ac/{digest}",
    "/bazel/ac/{digest}",
    mutable=True,
    content_addressed=False,
    feature=Feature.BAZEL,
)
BAZEL_CAS = Protocol(
    "bazel-cas",
    "/bazel/cas/",
    "/bazel/cas/{digest}",
    "/bazel/cas/{digest}",
    mutable=False,
    content_addressed=True,
    feature=Feature.BAZEL,
)
ALL = (GRADLE, MAVEN, BAZEL_AC, BAZEL_CAS)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()
