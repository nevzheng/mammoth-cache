"""A model of the cache as a dictionary, and Hypothesis state machines that hold the node to it.

`CacheModel` sends random sequences of `PUT`, `GET` and `HEAD` across every enabled protocol and
compares each answer with a dictionary of what was stored. `AuthModel` adds callers with different
credentials and scopes, and checks that each one gets exactly the access its scopes give, and that a
refused store changes nothing.

The node is shared with other tests and not wiped between examples, so every example salts its
keys and starts from an empty model. Content-addressed keys (Bazel CAS) cannot be salted, so a CAS
entry may already exist from an earlier example; the model allows for that.

On the main node nothing is evicted during a run: the examples write a few megabytes, far below the
memory tier's size. The evicting node's tiers hold only a few entries, but its stores wait for the
bucket upload, so an evicted entry is still read back from the bucket. Either way a miss on a key
the model holds is a bug, not eviction.
"""

from __future__ import annotations

import uuid
from collections import Counter, deque
from dataclasses import dataclass
from typing import TYPE_CHECKING, ClassVar, override

import httpx
from hypothesis import strategies as st
from hypothesis.stateful import Bundle, RuleBasedStateMachine, consumes, initialize, rule

from mammoth_e2e import blobs
from mammoth_e2e.metrics import Scrape, parse
from mammoth_e2e.protocols import BAZEL_CAS, GRADLE, Protocol, sha256

if TYPE_CHECKING:
    from mammoth_e2e.contract import Contracts
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Credentials, Target

HEX = "0123456789abcdef"
MAVEN_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-"
READ, WRITE = "cache:read", "cache:write"
LOOKUPS, STORES = "mammoth_cache_lookups_total", "mammoth_cache_stores_total"
AUTH_FAILURES = "mammoth_auth_failures_total"
# The label each outcome counter is broken down by, and that the model predicts.
OUTCOME_LABELS = {LOOKUPS: "result", STORES: "result", AUTH_FAILURES: "reason"}

# Small bodies most of the time, so runs stay fast; now and then one big enough to span many
# chunks on the way through hyper and the tiers.
SIZES = st.one_of(
    st.integers(0, 64),
    st.integers(0, 4096),
    st.integers(60_000, 300_000),
)
BODIES = st.builds(blobs.payload, SIZES, st.text(HEX, min_size=8, max_size=8))
MAVEN_SEGMENTS = st.lists(
    st.text(MAVEN_CHARS, min_size=1, max_size=24).filter(lambda s: s not in {".", ".."}),
    min_size=1,
    max_size=6,
)


@dataclass(frozen=True, slots=True)
class Key:
    protocol: Protocol
    name: str

    @property
    def path(self) -> str:
        return self.protocol.path(self.name)

    def __repr__(self) -> str:
        return f"Key({self.path!r})"


@dataclass(frozen=True, slots=True)
class Caller:
    """Who sends a request, and what the node should let them do."""

    label: str
    auth: httpx.Auth | None
    headers: dict[str, str]
    scopes: frozenset[str]
    known: bool  # credentials the node accepts; unknown ones get 401 whatever the scopes

    def __repr__(self) -> str:
        return f"Caller({self.label})"


def callers(target: Target) -> list[Caller]:
    """Anonymous, every configured user over Basic and Bearer, and one wrong password."""
    found = [Caller("anonymous", None, {}, target.anonymous_scopes, known=False)]
    for name, user in sorted(target.users.items()):
        token = user.token.get_secret_value()
        found.append(Caller(f"{name}:basic", _basic(user), {}, user.scopes, known=True))
        found.append(
            Caller(
                f"{name}:bearer",
                None,
                {"Authorization": f"Bearer {token}"},
                user.scopes,
                known=True,
            )
        )
    if target.users:
        name, user = min(target.users.items())
        wrong = httpx.BasicAuth(user.username, user.token.get_secret_value() + "x")
        found.append(Caller(f"{name}:wrong-password", wrong, {}, frozenset(), known=True))
    return found


def _basic(user: Credentials) -> httpx.BasicAuth:
    return httpx.BasicAuth(user.username, user.token.get_secret_value())


def writer_credentials(target: Target) -> Credentials | None:
    """A user that may read and write, or `None` on a target without one (an open node)."""
    for _, user in sorted(target.users.items()):
        if {"cache:read", "cache:write"} <= user.scopes:
            return user
    return None


def full_access(target: Target) -> Caller:
    """A caller that may read and write: an open node's anonymous caller, or a configured user."""
    for caller in callers(target):
        if {READ, WRITE} <= caller.scopes and caller.auth is not None:
            return caller
    if not target.users and not target.anonymous_scopes:
        return Caller("open", None, {}, frozenset({READ, WRITE}), known=False)
    raise LookupError(f"target {target.name!r} has no user with {READ} and {WRITE}")


def outcome(caller: Caller, method: str, status: int) -> tuple[str, str] | None:
    """The outcome counter a response moves, and its label (docs/user/metrics.md).

    Only `GET` is a lookup (`HEAD` asks the tiers for a size, not the cache for an entry), and a
    refused request never reaches the cache.
    """
    if status == 401:
        reason = "CREDENTIALS_MISSING" if caller.label == "anonymous" else "CREDENTIALS_INVALID"
        return AUTH_FAILURES, reason
    if status == 403:
        return AUTH_FAILURES, "SCOPE_MISSING"
    if method == "GET" and status in {200, 404}:
        return LOOKUPS, "hit" if status == 200 else "miss"
    if method == "PUT" and status in {200, 201, 400}:
        # A 400 is a store that failed, such as a CAS body that does not match its digest.
        return STORES, {201: "stored", 200: "exists", 400: "error"}[status]
    return None


class CacheModel(RuleBasedStateMachine):
    """The node, held to a dictionary of `Key -> bytes`. Set the class variables before running."""

    target: ClassVar[Target]
    contracts: ClassVar[Contracts]
    protocols: ClassVar[tuple[Protocol, ...]]
    owner: ClassVar[Caller]

    keys = Bundle("keys")

    def __init__(self) -> None:
        super().__init__()
        self.salt = uuid.uuid4().hex[:16]
        self.stored: dict[Key, bytes] = {}
        self.history: deque[str] = deque(maxlen=8)
        # What this example sent, as mammoth_http_requests_total counts it: (route, method, code).
        self.sent: Counter[tuple[str, str, str]] = Counter()
        # What the outcome counters should have moved by: (metric, label value).
        self.outcomes: Counter[tuple[str, str]] = Counter()
        self.baseline: Scrape | None = None
        self.last: Scrape | None = None
        self.http = httpx.Client(base_url=self.target.url("/"), timeout=httpx.Timeout(30.0))

    @override
    def teardown(self) -> None:
        self.http.close()

    # Sending, with every response checked against the contract.

    def send(
        self,
        caller: Caller,
        method: str,
        key: Key,
        body: bytes | None = None,
    ) -> httpx.Response:
        headers = dict(caller.headers)
        if key.protocol == GRADLE and method == "PUT":
            headers["Content-Type"] = "application/vnd.gradle.build-cache-artifact.v2"
        try:
            response = self.http.request(
                method,
                key.path,
                content=body,
                headers=headers,
                auth=caller.auth or httpx.USE_CLIENT_DEFAULT,
            )
        except httpx.TransportError as e:
            recent = "\n  ".join(self.history)
            raise AssertionError(
                f"{method} {key.path}: {e!r}; requests before it:\n  {recent}"
            ) from e
        self.sent[key.protocol.metrics_route, method, str(response.status_code)] += 1
        if (counted := outcome(caller, method, response.status_code)) is not None:
            self.outcomes[counted] += 1
        self.history.append(
            f"{caller.label} {method} {key.path} ({len(body or b'')} B) -> {response.status_code}"
        )
        self.contracts.check(key.protocol.template, response)
        return response

    def store(self, caller: Caller, key: Key, body: bytes) -> None:
        """`PUT` as an allowed caller, and update the model."""
        response = self.send(caller, "PUT", key, body)
        held = self.stored.get(key)
        if held is not None and not key.protocol.mutable:
            assert response.status_code == 200, f"{key}: re-store answered {response.status_code}"
            return
        if key.protocol.content_addressed and held is None:
            # It may exist from an earlier example; either answer is right.
            assert response.status_code in {200, 201}, (
                f"{key}: store answered {response.status_code}"
            )
        else:
            assert response.status_code == 201, f"{key}: store answered {response.status_code}"
        self.stored[key] = body

    def load(self, caller: Caller, key: Key) -> None:
        """`GET` as an allowed caller, and compare with the model."""
        response = self.send(caller, "GET", key)
        expected = self.stored.get(key)
        if expected is None and key.protocol.content_addressed and response.status_code == 200:
            # A blob from an earlier example: the bytes must still hash to the key.
            assert sha256(response.content) == key.name, f"{key}: hit does not hash to its key"
            return
        if expected is None:
            assert response.status_code == 404, (
                f"{key}: never stored, answered {response.status_code}"
            )
            return
        assert response.status_code == 200, f"{key}: stored, answered {response.status_code}"
        assert response.content == expected, (
            f"{key}: got {len(response.content)} bytes, stored {len(expected)}"
        )
        assert response.headers["content-type"] == "application/octet-stream"

    def check(self, caller: Caller, key: Key) -> None:
        """`HEAD` as an allowed caller, and compare with the model."""
        response = self.send(caller, "HEAD", key)
        expected = self.stored.get(key)
        if expected is None and key.protocol.content_addressed:
            assert response.status_code in {200, 404}
            return
        assert response.status_code == (404 if expected is None else 200), (
            f"{key}: HEAD answered {response.status_code}"
        )
        if expected is not None and "content-length" in response.headers:
            assert int(response.headers["content-length"]) == len(expected)

    # Rules.

    @initialize()
    def start(self) -> None:
        assert self.protocols, "no protocol enabled on this target"
        self.baseline = self.last = self.scrape()

    # Metrics: what the node counts must match what this example sent.

    def scrape(self) -> Scrape | None:
        """`/metrics`, as a user with `metrics:read` if any; `None` if it is closed to us."""
        reader = next((c for c in callers(self.target) if "metrics:read" in c.scopes), None)
        response = self.http.get(
            "/metrics",
            headers=reader.headers if reader else {},
            auth=(reader.auth if reader and reader.auth else httpx.USE_CLIENT_DEFAULT),
        )
        return parse(response.text) if response.status_code == 200 else None

    @rule()
    def metrics_add_up(self) -> None:
        """Counters never go down, and the request and outcome counters moved by exactly what
        this example sent: every hit, miss, store and refusal is counted once and labelled right.

        Assumes nothing else sends cache requests to the node while the test runs.
        """
        if self.baseline is None or self.last is None:
            return
        now = self.scrape()
        assert now is not None, "/metrics stopped answering"
        for (name, labels), value in self.last.samples.items():
            if name.endswith("_total"):
                current = now.samples.get((name, labels), 0.0)
                assert current >= value, f"{name}{dict(labels)} went from {value} to {current}"
        routes = {p.metrics_route for p in self.protocols}
        moved: Counter[tuple[str, str, str]] = Counter()
        for (name, labels), value in now.samples.items():
            if name != "mammoth_http_requests_total":
                continue
            label = dict(labels)
            if label.get("route") in routes:
                key = (label["route"], label["method"], label["code"])
                moved[key] += int(value - self.baseline.samples.get((name, labels), 0.0))
        moved = +moved  # drop zero entries
        assert moved == +self.sent, (
            f"mammoth_http_requests_total moved by {dict(moved)}, but this example sent "
            f"{dict(self.sent)}"
        )
        outcomes: Counter[tuple[str, str]] = Counter()
        for (name, labels), value in now.samples.items():
            if name in OUTCOME_LABELS:
                label = dict(labels)[OUTCOME_LABELS[name]]
                outcomes[name, label] += int(value - self.baseline.samples.get((name, labels), 0.0))
        outcomes = +outcomes
        assert outcomes == +self.outcomes, (
            f"outcome counters moved by {dict(outcomes)}, but this example's answers mean "
            f"{dict(self.outcomes)}"
        )
        self.last = now

    @rule(target=keys, data=st.data())
    def new_key(self, data: st.DataObject) -> Key:
        """A key nobody has used, in a protocol whose keys are names (not content hashes)."""
        named = [p for p in self.protocols if not p.content_addressed]
        protocol = data.draw(st.sampled_from(named), label="protocol")
        match protocol.name:
            case "gradle":
                name = self.salt + data.draw(st.text(HEX, max_size=112), label="suffix")
            case "maven":
                name = "/".join([self.salt, *data.draw(MAVEN_SEGMENTS, label="segments")])
            case _:
                name = sha256((self.salt + data.draw(st.text(HEX), label="seed")).encode())
        return Key(protocol, name)

    @rule(target=keys, key=keys, data=st.data())
    def same_name_elsewhere(self, key: Key, data: st.DataObject) -> Key:
        """The same name under another protocol: namespaces must never share entries."""
        if key.protocol.content_addressed:
            return key  # a CAS name is not salted, so it may be in use from an earlier example
        others = [
            p
            for p in self.protocols
            if p != key.protocol and not p.content_addressed and p.accepts(key.name)
        ]
        if not others:
            return key
        return Key(data.draw(st.sampled_from(others), label="protocol"), key.name)

    @rule(key=keys, body=BODIES)
    def put(self, key: Key, body: bytes) -> None:
        if key.protocol.content_addressed:
            return  # put_blob covers these; a named body under a CAS key is put_wrong_digest
        self.store(self.owner, key, body)

    @rule(target=keys, body=BODIES)
    def put_blob(self, body: bytes) -> Key:
        if BAZEL_CAS not in self.protocols:
            return Key(GRADLE, self.salt)
        key = Key(BAZEL_CAS, sha256(body))
        self.store(self.owner, key, body)
        return key

    @rule(body=BODIES, seed=st.text(HEX, max_size=8))
    def put_wrong_digest(self, body: bytes, seed: str) -> None:
        """A CAS upload that does not hash to its digest is refused and leaves nothing behind."""
        if BAZEL_CAS not in self.protocols:
            return
        key = Key(BAZEL_CAS, sha256(f"{self.salt}/{seed}/{len(body)}".encode()))
        if sha256(body) == key.name:
            return
        response = self.send(self.owner, "PUT", key, body)
        assert response.status_code == 400, (
            f"{key}: mismatched blob answered {response.status_code}"
        )
        assert response.json()["error"]["details"][0]["reason"] == "DIGEST_MISMATCH"
        self.load(self.owner, key)

    @rule(key=keys)
    def get(self, key: Key) -> None:
        self.load(self.owner, key)

    @rule(key=keys)
    def head(self, key: Key) -> None:
        self.check(self.owner, key)

    @rule(key=consumes(keys), body=BODIES)
    def put_then_get(self, key: Key, body: bytes) -> None:
        """Read your write: a store that says it kept the entry must serve it right away."""
        if key.protocol.content_addressed:
            key = Key(key.protocol, sha256(body))
        self.store(self.owner, key, body)
        self.load(self.owner, key)


class AuthModel(CacheModel):
    """`CacheModel`, plus requests from every kind of caller. Refused requests change nothing."""

    everyone: ClassVar[tuple[Caller, ...]]

    def expect_denial(self, caller: Caller, scope: str) -> int | None:
        """The status the node must refuse with, or `None` if `caller` may go ahead."""
        valid = caller.known and caller.label.split(":")[-1] != "wrong-password"
        if caller.label == "anonymous":
            return None if scope in caller.scopes else 401
        if not valid:
            return 401
        return None if scope in caller.scopes else 403

    def refused(self, response: httpx.Response, status: int) -> None:
        assert response.status_code == status, (
            f"{response.request.method} {response.request.url.path}: expected {status}, "
            f"got {response.status_code}"
        )
        if status == 401:
            assert "www-authenticate" in response.headers

    @rule(data=st.data(), key=CacheModel.keys, body=BODIES)
    def put_as(self, data: st.DataObject, key: Key, body: bytes) -> None:
        caller = data.draw(st.sampled_from(self.everyone), label="caller")
        if key.protocol.content_addressed:
            key = Key(key.protocol, sha256(body))
        denial = self.expect_denial(caller, WRITE)
        if denial is None:
            self.store(caller, key, body)
            return
        self.refused(self.send(caller, "PUT", key, body), denial)
        self.load(self.owner, key)  # the refused store changed nothing

    @rule(data=st.data(), key=CacheModel.keys, method=st.sampled_from(["GET", "HEAD"]))
    def read_as(self, data: st.DataObject, key: Key, method: str) -> None:
        caller = data.draw(st.sampled_from(self.everyone), label="caller")
        denial = self.expect_denial(caller, READ)
        if denial is None:
            (self.load if method == "GET" else self.check)(caller, key)
            return
        self.refused(self.send(caller, method, key), denial)


class ChaosModel(CacheModel):
    """`CacheModel`, plus node restarts and bucket outages, on a stack the harness controls.

    The node has a disk, so everything the model holds is on local disk: a restart must lose
    nothing, and while the bucket is down every read must still hit with the right bytes and
    every store must still succeed. Nothing may answer 5xx.
    """

    stack: ClassVar[Stack]
    has_bucket: ClassVar[bool]

    def __init__(self) -> None:
        super().__init__()
        self.bucket_down = False

    def reconnect(self) -> None:
        """A restarted node has dropped every connection; start over, and reset the counters."""
        self.http = httpx.Client(base_url=self.target.url("/"), timeout=httpx.Timeout(30.0))
        self.sent.clear()
        self.outcomes.clear()
        self.baseline = self.last = self.scrape()

    @rule()
    def restart_node(self) -> None:
        # Close our idle keep-alive connections first, or the graceful drain waits them out.
        self.http.close()
        self.stack.restart("mammoth")
        self.reconnect()

    @rule()
    def stop_bucket(self) -> None:
        if self.has_bucket and not self.bucket_down:
            self.stack.stop("seaweedfs")
            self.bucket_down = True

    @rule()
    def start_bucket(self) -> None:
        if self.bucket_down:
            self.stack.start("seaweedfs")
            self.bucket_down = False

    @override
    def teardown(self) -> None:
        self.start_bucket()
        super().teardown()
