"""A state machine that holds the gRPC (REAPI) and HTTP Bazel caches to one model.

Both serve the same entries (doc 0016): a blob or action result stored over one must be a hit over
the other. `ParityModel` uploads blobs over HTTP, `BatchUpdateBlobs` or ByteStream, reads them back
over all three (ByteStream with random offsets and limits), asks `FindMissingBlobs`, and stores and
reads action results over both. Every answer is compared with a model of the CAS and the AC.

Over gRPC, an action result whose outputs are not all in the CAS is a miss, so a client that trusts
it never asks for a blob that is gone; over HTTP the stored bytes are served as they are. Results
may list output directories, as `Tree` blobs whose files are in the CAS.

Batches carry up to eight blobs, some corrupt, and batch reads past the size limit are refused
whole.
`FindMissingBlobs` also gets thousands of digests at once, `QueryWriteStatus` must agree with the
model, and calls switch between instance names, which the node accepts and ignores.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from typing import TYPE_CHECKING, ClassVar, override

import grpc
import httpx
from hypothesis import strategies as st
from hypothesis.stateful import Bundle, RuleBasedStateMachine, initialize, rule

from mammoth_e2e import blobs
from mammoth_e2e.reapi import Message, Reapi, attempt, sha256

if TYPE_CHECKING:
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.model import Caller
    from mammoth_e2e.target import Target

OK, INVALID_ARGUMENT, NOT_FOUND = (
    grpc.StatusCode.OK.value[0],
    grpc.StatusCode.INVALID_ARGUMENT.value[0],
    grpc.StatusCode.NOT_FOUND.value[0],
)
WAYS = ("http", "batch", "bytestream")
SIZES = st.one_of(st.integers(0, 64), st.integers(0, 4096), st.integers(50_000, 600_000))
MAX_BATCH = 4 * 1024 * 1024  # max_batch_total_size_bytes
INSTANCES = st.sampled_from(["", "main", "ci/linux-x86_64"])


@dataclass(frozen=True, slots=True)
class Blob:
    data: bytes

    @property
    def hash(self) -> str:
        return sha256(self.data)

    @property
    def size(self) -> int:
        return len(self.data)

    def __repr__(self) -> str:
        return f"Blob({self.hash[:12]}…, {self.size} B)"


@dataclass(frozen=True, slots=True)
class Action:
    hash: str
    size: int

    def __repr__(self) -> str:
        return f"Action({self.hash[:12]}…)"


class ParityModel(RuleBasedStateMachine):
    """The node's CAS and AC, held to sets and dicts. Set the class variables before running."""

    target: ClassVar[Target]
    contracts: ClassVar[Contracts]
    owner: ClassVar[Caller]  # may read and write over HTTP
    authorization: ClassVar[str | None]  # the same user, as gRPC metadata

    blobs = Bundle("blobs")
    actions = Bundle("actions")

    def __init__(self) -> None:
        super().__init__()
        self.salt = uuid.uuid4().hex.encode()
        self.cas: set[str] = {sha256(b"")}  # the empty blob is always present
        self.ac: dict[str, Message] = {}
        assert self.target.reapi_address is not None
        self.grpc = Reapi(self.target.reapi_address, self.authorization)
        self.http = httpx.Client(
            base_url=self.target.url("/"),
            timeout=httpx.Timeout(30.0),
            headers=self.owner.headers,
            auth=self.owner.auth,
        )

    @override
    def teardown(self) -> None:
        self.grpc.close()
        self.http.close()

    def digest(self, blob: Blob) -> Message:
        return self.grpc.s.re.Digest(hash=blob.hash, size_bytes=blob.size)

    def http_send(self, method: str, path: str, body: bytes | None = None) -> httpx.Response:
        response = self.http.request(method, path, content=body)
        template = "/bazel/cas/{digest}" if path.startswith("/bazel/cas/") else "/bazel/ac/{digest}"
        self.contracts.check(template, response)
        return response

    @initialize()
    def start(self) -> None:
        assert self.target.reapi_address, "the target has no reapi_address"

    # Seeding both bundles means every rule can run from the first step; with them empty,
    # Hypothesis discards so many rule choices that it fails its filter_too_much health check.

    @initialize(target=blobs, size=SIZES, seed=st.text("0123456789abcdef", min_size=4, max_size=4))
    def first_blob(self, size: int, seed: str) -> Blob:
        return self.blob(size, seed)

    @initialize(target=actions, seed=st.text("0123456789abcdef", max_size=8))
    def first_action(self, seed: str) -> Action:
        return self.action(seed)

    # Blobs

    @rule(target=blobs, size=SIZES, seed=st.text("0123456789abcdef", min_size=4, max_size=4))
    def new_blob(self, size: int, seed: str) -> Blob:
        return self.blob(size, seed)

    def blob(self, size: int, seed: str) -> Blob:
        """A blob nobody has stored: salted, except the empty one."""
        if size == 0:
            return Blob(b"")
        return Blob(self.salt + blobs.payload(size, f"{self.salt.decode()}/{seed}/{size}"))

    @rule(blob=blobs, way=st.sampled_from(WAYS), chunk=st.integers(1, 256 * 1024))
    def upload(self, blob: Blob, way: str, chunk: int) -> None:
        held = blob.hash in self.cas
        match way:
            case "http":
                status = self.http_send("PUT", f"/bazel/cas/{blob.hash}", blob.data).status_code
                expected = {200} if held and blob.size else {200, 201}
                assert status in expected, f"{blob}: HTTP store answered {status}"
            case "batch":
                (code,) = self.grpc.batch_update([(self.digest(blob), blob.data)])
                assert code == OK, f"{blob}: BatchUpdateBlobs answered {code}"
            case _:
                committed = self.grpc.write(self.digest(blob), blob.data, chunk)
                assert committed == blob.size, f"{blob}: Write committed {committed}"
        self.cas.add(blob.hash)

    @rule(blob=blobs, way=st.sampled_from(WAYS), flip=st.integers(0, 2**31))
    def upload_corrupt(self, blob: Blob, way: str, flip: int) -> None:
        """Bytes that do not hash to the digest are refused and leave nothing behind."""
        if blob.size == 0:
            return
        data = bytearray(blob.data)
        data[flip % blob.size] ^= 0x01
        match way:
            case "http":
                status = self.http_send("PUT", f"/bazel/cas/{blob.hash}", bytes(data)).status_code
                if blob.hash in self.cas:
                    # Already stored: the node may keep its copy without reading the body.
                    assert status in {200, 400}, f"{blob}: corrupt re-store answered {status}"
                else:
                    assert status == 400, f"{blob}: corrupt HTTP store answered {status}"
            case "batch":
                (code,) = self.grpc.batch_update([(self.digest(blob), bytes(data))])
                if blob.hash not in self.cas:
                    assert code == INVALID_ARGUMENT, f"{blob}: corrupt batch item answered {code}"
            case _:
                code, _ = attempt(
                    lambda: self.grpc.write(self.digest(blob), bytes(data), 64 * 1024)
                )
                if code == grpc.StatusCode.OK:
                    assert blob.hash in self.cas, f"{blob}: a corrupt Write was accepted"
                else:
                    assert code == grpc.StatusCode.INVALID_ARGUMENT, f"{blob}: corrupt Write {code}"
        self.read(blob, "bytestream", 0, 0)

    @rule(blob=blobs, way=st.sampled_from(WAYS), offset=st.integers(0), limit=st.integers(0))
    def read(self, blob: Blob, way: str, offset: int, limit: int) -> None:
        held = blob.hash in self.cas
        match way:
            case "http":
                response = self.http_send("GET", f"/bazel/cas/{blob.hash}")
                if held:
                    assert response.status_code == 200, f"{blob}: HTTP read {response.status_code}"
                    assert response.content == blob.data, f"{blob}: HTTP read wrong bytes"
                else:
                    assert response.status_code == 404, f"{blob}: HTTP read {response.status_code}"
            case "batch":
                ((code, data),) = self.grpc.batch_read([self.digest(blob)])
                if held:
                    assert (code, data) == (OK, blob.data), f"{blob}: BatchReadBlobs code {code}"
                else:
                    assert code == NOT_FOUND, f"{blob}: BatchReadBlobs code {code}"
            case _:
                offset %= blob.size + 1
                limit %= blob.size + 1
                wanted = blob.data[offset : offset + limit] if limit else blob.data[offset:]
                code, data = attempt(lambda: self.grpc.read(self.digest(blob), offset, limit))
                if code != grpc.StatusCode.OK:
                    assert not held, f"{blob}: Read at {offset}+{limit} failed with {code}"
                    assert code == grpc.StatusCode.NOT_FOUND, f"{blob}: Read failed with {code}"
                    return
                # Nothing to send needs no lookup, so an empty window answers even on a miss.
                assert held or not wanted, f"{blob}: Read of a missing blob answered"
                assert data == wanted, f"{blob}: Read at {offset}+{limit} got wrong bytes"

    @rule(found=st.lists(blobs, min_size=1, max_size=8))
    def find_missing(self, found: list[Blob]) -> None:
        missing = self.grpc.find_missing([self.digest(b) for b in found])
        expected = {(b.hash, b.size) for b in found if b.hash not in self.cas}
        assert missing == expected, f"FindMissingBlobs said {missing}, the model {expected}"

    @rule(found=st.lists(blobs, max_size=8), unknown=st.integers(500, 3000))
    def find_missing_many(self, found: list[Blob], unknown: int) -> None:
        """A large build asks about thousands of digests in one call."""
        fresh = [sha256(self.salt + b"/unknown/%d" % i) for i in range(unknown)]
        digests = [self.digest(b) for b in found]
        digests += [self.grpc.s.re.Digest(hash=h, size_bytes=1) for h in fresh]
        missing = self.grpc.find_missing(digests)
        expected = {(b.hash, b.size) for b in found if b.hash not in self.cas}
        expected |= {(h, 1) for h in fresh}
        assert missing == expected, f"FindMissingBlobs missed {len(expected ^ missing)} digests"

    @rule(
        drawn=st.lists(blobs, min_size=1, max_size=8),
        corrupt=st.sets(st.integers(0, 7)),
    )
    def batch_upload(self, drawn: list[Blob], corrupt: set[int]) -> None:
        """Each blob in a batch gets its own status; a corrupt one fails alone."""
        # Deduplicated here rather than with `unique_by`, which a small bundle makes Hypothesis
        # filter until it fails its health check.
        found = list({b.hash: b for b in drawn}.values())
        if sum(b.size for b in found) > MAX_BATCH:
            return
        items: list[tuple[Message, bytes]] = []
        bad: set[int] = set()
        for i, blob in enumerate(found):
            data = blob.data
            if i in corrupt and blob.size:
                data = bytes([data[0] ^ 0x01]) + data[1:]
                bad.add(i)
            items.append((self.digest(blob), data))
        codes = self.grpc.batch_update(items)
        for i, (blob, code) in enumerate(zip(found, codes, strict=True)):
            if i not in bad:
                assert code == OK, f"{blob}: batch item answered {code}"
                self.cas.add(blob.hash)
            elif blob.hash not in self.cas:
                assert code == INVALID_ARGUMENT, f"{blob}: corrupt batch item answered {code}"

    @rule(found=st.lists(blobs, min_size=1, max_size=8))
    def batch_read_many(self, found: list[Blob]) -> None:
        if sum(b.size for b in found) > MAX_BATCH:
            return
        answers = self.grpc.batch_read([self.digest(b) for b in found])
        for blob, (code, data) in zip(found, answers, strict=True):
            if blob.hash in self.cas:
                assert (code, data) == (OK, blob.data), f"{blob}: batch read code {code}"
            else:
                assert code == NOT_FOUND, f"{blob}: batch read code {code}"

    @rule(found=st.lists(blobs, max_size=4), claimed=st.integers(MAX_BATCH + 1, 2**40))
    def batch_read_too_big(self, found: list[Blob], claimed: int) -> None:
        """A batch read that could not fit in a response is refused whole, before any lookup."""
        digests = [self.digest(b) for b in found]
        digests.append(self.grpc.s.re.Digest(hash=sha256(self.salt), size_bytes=claimed))
        code, _ = attempt(lambda: self.grpc.batch_read(digests))
        assert code == grpc.StatusCode.INVALID_ARGUMENT, f"oversized batch read answered {code}"

    @rule(blob=blobs)
    def query_write_status(self, blob: Blob) -> None:
        """Uploads are not resumable: a blob is committed whole or not at all."""
        committed, complete = self.grpc.query_write_status(self.digest(blob))
        if blob.hash in self.cas:
            assert (committed, complete) == (blob.size, True), f"{blob}: {committed}, {complete}"
        else:
            assert (committed, complete) == (0, False), f"{blob}: {committed}, {complete}"

    @rule(instance=INSTANCES)
    def switch_instance(self, instance: str) -> None:
        """Every instance name maps to the same cache (doc 0016)."""
        self.grpc.instance = instance

    # Action results

    @rule(target=actions, seed=st.text("0123456789abcdef", max_size=8))
    def new_action(self, seed: str) -> Action:
        return self.action(seed)

    def action(self, seed: str) -> Action:
        command = self.salt + b"/action/" + seed.encode()
        return Action(sha256(command), len(command))

    @rule(target=blobs, files=st.lists(blobs, max_size=4))
    def new_tree(self, files: list[Blob]) -> Blob:
        """A `Tree` of one directory, as a blob. Only files the CAS holds, so that walking the
        tree or not (doc 0016, open question) gives the same answer: nothing is ever evicted."""
        re = self.grpc.s.re
        held = [b for b in {b.hash: b for b in files}.values() if b.hash in self.cas]
        # An empty file named after the salt (the empty blob is always present) keeps this run's
        # trees apart from every other run's.
        nodes = [re.FileNode(name=self.salt.decode(), digest=self.digest(Blob(b"")))]
        nodes += [re.FileNode(name=f"f{i}", digest=self.digest(b)) for i, b in enumerate(held)]
        root = re.Directory(files=nodes)
        return Blob(re.Tree(root=root).SerializeToString(deterministic=True))

    @rule(
        action=actions,
        outputs=st.lists(blobs, max_size=3),
        trees=st.lists(blobs, max_size=2),
        stdout=st.none() | blobs,
        exit_code=st.integers(0, 2),
        way=st.sampled_from(("http", "grpc")),
    )
    def store_result(  # noqa: PLR0913, PLR0917 - one argument per drawn part of the result
        self,
        action: Action,
        outputs: list[Blob],
        trees: list[Blob],
        stdout: Blob | None,
        exit_code: int,
        way: str,
    ) -> None:
        re = self.grpc.s.re
        result = re.ActionResult(
            output_files=[
                re.OutputFile(path=f"out/{i}", digest=self.digest(b)) for i, b in enumerate(outputs)
            ],
            output_directories=[
                re.OutputDirectory(path=f"dir/{i}", tree_digest=self.digest(t))
                for i, t in enumerate(trees)
            ],
            exit_code=exit_code,
        )
        if stdout is not None:
            result.stdout_digest.CopyFrom(self.digest(stdout))
        if way == "http":
            response = self.http_send("PUT", f"/bazel/ac/{action.hash}", result.SerializeToString())
            assert response.status_code == 201, f"{action}: HTTP store {response.status_code}"
        else:
            digest = re.Digest(hash=action.hash, size_bytes=action.size)
            assert self.grpc.update_action_result(digest, result) == result
        self.ac[action.hash] = result

    @rule(action=actions, way=st.sampled_from(("http", "grpc")))
    def get_result(self, action: Action, way: str) -> None:
        re = self.grpc.s.re
        stored = self.ac.get(action.hash)
        if way == "http":
            response = self.http_send("GET", f"/bazel/ac/{action.hash}")
            if stored is None:
                assert response.status_code == 404, f"{action}: HTTP read {response.status_code}"
                return
            assert response.status_code == 200, f"{action}: HTTP read {response.status_code}"
            assert re.ActionResult.FromString(response.content) == stored
            return
        digest = re.Digest(hash=action.hash, size_bytes=action.size)
        referenced = [f.digest.hash for f in stored.output_files] if stored else []
        referenced += [d.tree_digest.hash for d in stored.output_directories] if stored else []
        if stored is not None and stored.HasField("stdout_digest"):
            referenced.append(stored.stdout_digest.hash)
        complete = stored is not None and all(h in self.cas for h in referenced)
        code, got = attempt(lambda: self.grpc.get_action_result(digest))
        if code != grpc.StatusCode.OK:
            assert code == grpc.StatusCode.NOT_FOUND, f"{action}: GetActionResult {code}"
            assert not complete, f"{action}: stored with every output, answered NOT_FOUND"
            return
        assert complete, f"{action}: answered although an output is missing or never stored"
        assert got == stored, f"{action}: GetActionResult answered a different result"
