"""A small REAPI v2 cache client over gRPC, for tests of the node's gRPC listener (doc 0016).

The Python stubs are generated from the protos vendored in `third_party/proto` the first time they
are needed, into the harness cache, so nothing generated is checked in. `google.api`,
`google.longrunning` and `google.rpc` come from googleapis-common-protos.
"""

from __future__ import annotations

import hashlib
import importlib
import sys
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

import grpc

from mammoth_e2e.tools import cache_dir

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator
    from types import ModuleType

PROTO_DIR = Path(__file__).resolve().parents[3] / "third_party" / "proto"
PROTOS = (
    "build/bazel/remote/execution/v2/remote_execution.proto",
    "build/bazel/semver/semver.proto",
    "google/bytestream/bytestream.proto",
)

type Message = Any  # generated protobuf messages have no static types here


@dataclass(frozen=True, slots=True)
class Stubs:
    """The generated modules: `re` (REAPI messages), `re_grpc`, `bs` (ByteStream), `bs_grpc`."""

    re: ModuleType
    re_grpc: ModuleType
    bs: ModuleType
    bs_grpc: ModuleType


def _generate(out: Path) -> None:
    from grpc_tools import protoc  # noqa: PLC0415 - only needed to generate

    well_known = Path(protoc.__file__).parent / "_proto"
    out.mkdir(parents=True, exist_ok=True)
    code = protoc.main(
        [
            "protoc",
            f"-I{PROTO_DIR}",
            f"-I{well_known}",
            f"--python_out={out}",
            f"--grpc_python_out={out}",
            *PROTOS,
        ]
    )
    if code != 0:
        raise RuntimeError(f"protoc failed with {code} on {PROTO_DIR}")


def stubs() -> Stubs:
    """Generates the stubs once per proto revision, and imports them."""
    revision = hashlib.sha256()
    for proto in PROTOS:
        revision.update((PROTO_DIR / proto).read_bytes())
    out = cache_dir() / "reapi-stubs" / revision.hexdigest()[:16]
    if not (out / "google" / "bytestream" / "bytestream_pb2_grpc.py").is_file():
        _generate(out)
    if str(out) not in sys.path:
        sys.path.insert(0, str(out))
    v2 = "build.bazel.remote.execution.v2"
    return Stubs(
        re=importlib.import_module(f"{v2}.remote_execution_pb2"),
        re_grpc=importlib.import_module(f"{v2}.remote_execution_pb2_grpc"),
        bs=importlib.import_module("google.bytestream.bytestream_pb2"),
        bs_grpc=importlib.import_module("google.bytestream.bytestream_pb2_grpc"),
    )


def attempt[T](call: Callable[[], T]) -> tuple[grpc.StatusCode, T | None]:
    """Runs a call: its status code, and its result when it succeeded."""
    try:
        return grpc.StatusCode.OK, call()
    except grpc.RpcError as e:
        return e.code(), None


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@dataclass(slots=True)
class Reapi:
    """The cache services on one node. `authorization` is sent as metadata on every call, and
    `instance` is the instance name of every request (the node accepts and ignores it)."""

    address: str
    authorization: str | None = None
    instance: str = ""
    # PEM CA certificates to trust; set for a node with [tls], which serves gRPC over TLS.
    root_certificates: bytes | None = None
    s: Stubs = field(default_factory=stubs)
    _channel: grpc.Channel = field(init=False)
    _cas: Any = field(init=False)
    _ac: Any = field(init=False)
    _bytestream: Any = field(init=False)

    def __post_init__(self) -> None:
        options = [
            ("grpc.max_receive_message_length", 64 * 1024 * 1024),
            ("grpc.max_send_message_length", 64 * 1024 * 1024),
        ]
        if self.root_certificates is None:
            self._channel = grpc.insecure_channel(self.address, options=options)
        else:
            credentials = grpc.ssl_channel_credentials(root_certificates=self.root_certificates)
            self._channel = grpc.secure_channel(self.address, credentials, options=options)
        self._cas = self.s.re_grpc.ContentAddressableStorageStub(self._channel)
        self._ac = self.s.re_grpc.ActionCacheStub(self._channel)
        self._bytestream = self.s.bs_grpc.ByteStreamStub(self._channel)

    def close(self) -> None:
        self._channel.close()

    @property
    def metadata(self) -> list[tuple[str, str]]:
        return [("authorization", self.authorization)] if self.authorization else []

    def digest(self, data: bytes) -> Message:
        return self.s.re.Digest(hash=sha256(data), size_bytes=len(data))

    def _resource(self, rest: str) -> str:
        return f"{self.instance}/{rest}" if self.instance else rest

    # ContentAddressableStorage

    def find_missing(self, digests: list[Message]) -> set[tuple[str, int]]:
        request = self.s.re.FindMissingBlobsRequest(
            instance_name=self.instance, blob_digests=digests
        )
        response = self._cas.FindMissingBlobs(request, metadata=self.metadata, timeout=30)
        return {(d.hash, d.size_bytes) for d in response.missing_blob_digests}

    def batch_update(self, items: list[tuple[Message, bytes]]) -> list[int]:
        """Uploads blobs in one call; the `grpc.StatusCode` value of each, in order."""
        request = self.s.re.BatchUpdateBlobsRequest(
            instance_name=self.instance,
            requests=[
                self.s.re.BatchUpdateBlobsRequest.Request(digest=d, data=data) for d, data in items
            ],
        )
        response = self._cas.BatchUpdateBlobs(request, metadata=self.metadata, timeout=30)
        return [r.status.code for r in response.responses]

    def batch_read(self, digests: list[Message]) -> list[tuple[int, bytes]]:
        """Reads blobs in one call: each one's status code and data, in order."""
        request = self.s.re.BatchReadBlobsRequest(instance_name=self.instance, digests=digests)
        response = self._cas.BatchReadBlobs(request, metadata=self.metadata, timeout=30)
        return [(r.status.code, bytes(r.data)) for r in response.responses]

    # ByteStream

    def read(self, digest: Message, offset: int = 0, limit: int = 0) -> bytes:
        """Reads a blob, or part of one; raises `grpc.RpcError` on a miss."""
        request = self.s.bs.ReadRequest(
            resource_name=self._resource(f"blobs/{digest.hash}/{digest.size_bytes}"),
            read_offset=offset,
            read_limit=limit,
        )
        chunks = self._bytestream.Read(request, metadata=self.metadata, timeout=30)
        return b"".join(bytes(c.data) for c in chunks)

    def write(self, digest: Message, data: bytes, chunk: int) -> int:
        """Uploads `data` under `digest` in chunks; the committed size. Raises on refusal."""
        name = self._resource(f"uploads/{uuid.uuid4()}/blobs/{digest.hash}/{digest.size_bytes}")

        def requests() -> Iterator[Message]:
            if not data:
                yield self.s.bs.WriteRequest(resource_name=name, finish_write=True)
                return
            for offset in range(0, len(data), chunk):
                yield self.s.bs.WriteRequest(
                    resource_name=name if offset == 0 else "",
                    write_offset=offset,
                    data=data[offset : offset + chunk],
                    finish_write=offset + chunk >= len(data),
                )

        response = self._bytestream.Write(requests(), metadata=self.metadata, timeout=30)
        return int(response.committed_size)

    def query_write_status(self, digest: Message) -> tuple[int, bool]:
        """`committed_size` and `complete` for an upload of `digest` that never started."""
        name = self._resource(f"uploads/{uuid.uuid4()}/blobs/{digest.hash}/{digest.size_bytes}")
        request = self.s.bs.QueryWriteStatusRequest(resource_name=name)
        response = self._bytestream.QueryWriteStatus(request, metadata=self.metadata, timeout=30)
        return int(response.committed_size), bool(response.complete)

    # ActionCache

    def get_action_result(self, action: Message) -> Message:
        """The stored result for `action`; raises `grpc.RpcError` (NOT_FOUND) on a miss."""
        request = self.s.re.GetActionResultRequest(
            instance_name=self.instance, action_digest=action
        )
        return self._ac.GetActionResult(request, metadata=self.metadata, timeout=30)

    def update_action_result(self, action: Message, result: Message) -> Message:
        request = self.s.re.UpdateActionResultRequest(
            instance_name=self.instance, action_digest=action, action_result=result
        )
        return self._ac.UpdateActionResult(request, metadata=self.metadata, timeout=30)
