"""The OpenAPI contracts in `api/openapi/`, used as a response oracle.

`Contracts.check` fails a test when a response's status is not one the contract lists for that
operation, when an error body does not match the schema the contract gives for that status, or when
a header the contract pins to a constant (such as `WWW-Authenticate` on `401`) is missing or
different. It checks responses only; schemathesis covers requests the contract allows.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast
from urllib.parse import urldefrag, urljoin

import yaml
from jsonschema import Draft202012Validator
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012

if TYPE_CHECKING:
    import httpx

CONTRACT_DIR = Path(__file__).resolve().parents[3] / "api" / "openapi"
BASE_URI = "https://contracts.mammoth-cache.dev/openapi/"

type Json = Any


class ContractViolationError(AssertionError):
    """A response the contract does not allow."""


@dataclass(frozen=True, slots=True)
class _Node:
    uri: str  # the document's URI
    pointer: str  # a JSON pointer into it, starting with "/"
    value: Json


def _pointer_get(document: Json, pointer: str) -> Json:
    node = document
    for raw in pointer.lstrip("/").split("/") if pointer.strip("/") else []:
        part = raw.replace("~1", "/").replace("~0", "~")
        node = node[part]
    return node


def _escape(part: str) -> str:
    return part.replace("~", "~0").replace("/", "~1")


@dataclass(slots=True)
class Contracts:
    """Every OpenAPI document in a directory, indexed by path template."""

    directory: Path = CONTRACT_DIR
    _documents: dict[str, Json] = field(init=False, default_factory=dict)
    _paths: dict[str, str] = field(init=False, default_factory=dict)  # template -> document URI
    _registry: Registry[Json] = field(init=False)

    def __post_init__(self) -> None:
        resources: list[tuple[str, Resource[Json]]] = []
        for file in sorted(self.directory.glob("*.yaml")):
            uri = urljoin(BASE_URI, file.name)
            document = yaml.safe_load(file.read_text())
            self._documents[uri] = document
            resources.append((uri, DRAFT202012.create_resource(document)))
            for template in document.get("paths") or {}:
                self._paths[template] = uri
        self._registry = Registry().with_resources(resources)

    def has(self, template: str) -> bool:
        return template in self._paths

    def _resolve(self, node: _Node) -> _Node:
        """Follows `$ref` until it reaches a node that is not a reference."""
        while isinstance(node.value, dict) and "$ref" in node.value:
            target = urljoin(node.uri, cast("str", node.value["$ref"]))
            uri, fragment = urldefrag(target)
            node = _Node(uri, fragment, _pointer_get(self._documents[uri], fragment))
        return node

    def check(self, template: str, response: httpx.Response) -> None:
        """Raises `ContractViolationError` unless the contract allows `response` for `template`."""
        uri = self._paths.get(template)
        if uri is None:
            raise ContractViolationError(f"no contract describes {template}")
        method = response.request.method.lower()
        status = str(response.status_code)
        where = f"{response.request.method} {template} -> {status}"
        operation = self._documents[uri]["paths"][template].get(method)
        if operation is None:
            raise ContractViolationError(f"{where}: the contract has no {method} operation")
        responses = operation.get("responses") or {}
        if status not in responses:
            raise ContractViolationError(
                f"{where}: status not in the contract (allowed: {', '.join(sorted(responses))})"
                f"; body: {response.text[:300]!r}"
            )
        pointer = f"/paths/{_escape(template)}/{method}/responses/{status}"
        spec = self._resolve(_Node(uri, pointer, responses[status]))
        self._check_headers(where, spec, response)
        if method != "head":
            self._check_body(where, spec, response)

    def _check_headers(self, where: str, spec: _Node, response: httpx.Response) -> None:
        for name, header in (spec.value.get("headers") or {}).items():
            resolved = self._resolve(_Node(spec.uri, "", header))
            schema = self._resolve(_Node(resolved.uri, "", resolved.value.get("schema") or {}))
            if "const" in schema.value and response.headers.get(name) != schema.value["const"]:
                raise ContractViolationError(
                    f"{where}: header {name} is {response.headers.get(name)!r}, "
                    f"contract says {schema.value['const']!r}"
                )

    def _check_body(self, where: str, spec: _Node, response: httpx.Response) -> None:
        content = spec.value.get("content") or {}
        media = content.get("application/json")
        if media is None or "schema" not in media:
            return
        if not response.headers.get("content-type", "").startswith("application/json"):
            raise ContractViolationError(
                f"{where}: contract gives a JSON body, got {response.headers.get('content-type')!r}"
            )
        escaped = "/".join(
            [spec.pointer.rstrip("/"), "content", _escape("application/json"), "schema"]
        )
        validator = Draft202012Validator({"$ref": f"{spec.uri}#{escaped}"}, registry=self._registry)
        errors = sorted(validator.iter_errors(response.json()), key=lambda e: list(e.path))
        if errors:
            raise ContractViolationError(
                f"{where}: body breaks the contract: {errors[0].message}; "
                f"body: {response.text[:300]!r}"
            )
