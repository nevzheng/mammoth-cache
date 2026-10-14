"""Authentication and scopes on every route, end to end (doc 0002).

Happy paths, refusals, and malformed or hostile credentials. Every refusal must be a clean 401 or
403 with an AIP-193 body, never a 5xx, never a hit, never the token echoed back.
"""

from __future__ import annotations

import base64
from typing import TYPE_CHECKING

import httpx
import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st
from pydantic import SecretStr

from mammoth_e2e import blobs, raw
from mammoth_e2e.client import Auth, CacheClient, basic

if TYPE_CHECKING:
    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Credentials, Target

pytestmark = [pytest.mark.requires("auth"), pytest.mark.usefixtures("live")]

REALM = 'Basic realm="mammoth-cache"'


def reason(response: httpx.Response) -> str:
    return str(response.json()["error"]["details"][0]["reason"])


def assert_refused(response: httpx.Response, status: int, why: str, *secrets: str) -> None:
    assert response.status_code == status, response.text
    assert reason(response) == why, response.text
    if status == 401:
        assert response.headers.get("www-authenticate") == REALM
    for secret in secrets:
        assert secret not in response.text, "a token was echoed back"


@pytest.fixture
def stored(target: Target) -> tuple[str, str, bytes]:
    """A Gradle key and Maven path already stored by the read-write user."""
    writer = CacheClient(target, credentials=target.user("ci"))
    key, path, body = blobs.gradle_key(), blobs.maven_path(), blobs.payload(256)
    assert writer.gradle_put(key, body).status_code == 201
    assert writer.maven_put(path, body).status_code == 201
    writer.close()
    return key, path, body


def as_user(
    target: Target, credentials: Credentials | None, auth: Auth = Auth.BASIC
) -> CacheClient:
    return CacheClient(target, credentials=credentials, auth=auth)


# Happy paths


@pytest.mark.parametrize("auth", list(Auth), ids=lambda a: a.value)
def test_read_write_user_round_trips(target: Target, auth: Auth) -> None:
    client = as_user(target, target.user("ci"), auth)
    key, path, body = blobs.gradle_key(), blobs.maven_path(), blobs.payload(100)
    assert client.gradle_put(key, body).status_code == 201
    assert client.gradle_get(key).content == body
    assert client.maven_put(path, body).status_code == 201
    assert client.maven_get(path).content == body


@pytest.mark.parametrize("auth", list(Auth), ids=lambda a: a.value)
def test_read_only_user_reads(target: Target, stored: tuple[str, str, bytes], auth: Auth) -> None:
    key, path, body = stored
    client = as_user(target, target.user("developer"), auth)
    assert client.gradle_get(key).content == body
    assert client.head(f"/cache/{key}").status_code == 200
    assert client.maven_get(path).content == body


def test_health_needs_no_credentials(target: Target) -> None:
    for path in ("/ping", "/livez", "/readyz"):
        assert httpx.get(target.url(path)).status_code == 200


def test_metrics_open_by_default(target: Target) -> None:
    assert httpx.get(target.url("/metrics")).status_code == 200


def test_admin_api_with_admin_read(target: Target) -> None:
    client = as_user(target, target.user("admin"), Auth.BEARER)
    assert client.get("/v1/serverStatus").status_code == 200


def test_anonymous_read_on_open_node(
    target: Target, open_base_url: str, stored: tuple[str, str, bytes]
) -> None:
    writer = CacheClient(target, credentials=target.user("ci"), base_url=open_base_url)
    key, body = blobs.gradle_key(), blobs.payload(64)
    assert writer.gradle_put(key, body).status_code == 201
    anonymous = CacheClient(target, base_url=open_base_url)
    assert anonymous.gradle_get(key).content == body
    del stored


# Refusals


def test_no_credentials(target: Target, stored: tuple[str, str, bytes]) -> None:
    key, path, _ = stored
    client = as_user(target, None)
    assert_refused(client.gradle_get(key), 401, "CREDENTIALS_MISSING")
    assert_refused(client.maven_get(path), 401, "CREDENTIALS_MISSING")
    assert_refused(client.gradle_put(blobs.gradle_key(), b"x"), 401, "CREDENTIALS_MISSING")


def test_anonymous_write_on_open_node(target: Target, open_base_url: str) -> None:
    client = CacheClient(target, base_url=open_base_url)
    key = blobs.gradle_key()
    assert_refused(client.gradle_put(key, b"x"), 401, "CREDENTIALS_MISSING")
    assert (
        CacheClient(target, credentials=target.user("ci"), base_url=open_base_url)
        .gradle_get(key)
        .status_code
        == 404
    )


@pytest.mark.parametrize("auth", list(Auth), ids=lambda a: a.value)
def test_wrong_token(target: Target, stored: tuple[str, str, bytes], auth: Auth) -> None:
    ci = target.user("ci")
    wrong = ci.model_copy(update={"token": SecretStr(ci.token.get_secret_value() + "x")})
    client = as_user(target, wrong, auth)
    secret = wrong.token.get_secret_value()
    assert_refused(client.gradle_get(stored[0]), 401, "CREDENTIALS_INVALID", secret)


def test_unknown_user(target: Target, stored: tuple[str, str, bytes]) -> None:
    ci = target.user("ci")
    stranger = ci.model_copy(update={"username": "nobody"})
    assert_refused(as_user(target, stranger).gradle_get(stored[0]), 401, "CREDENTIALS_INVALID")


def test_token_of_another_user(target: Target, stored: tuple[str, str, bytes]) -> None:
    """Basic names a user; the token must be that user's, not anyone's."""
    swapped = target.user("developer").model_copy(update={"username": "ci"})
    assert_refused(as_user(target, swapped).gradle_get(stored[0]), 401, "CREDENTIALS_INVALID")


def test_read_only_user_cannot_write(target: Target) -> None:
    client = as_user(target, target.user("developer"))
    key = blobs.gradle_key()
    assert_refused(client.gradle_put(key, b"x"), 403, "SCOPE_MISSING")
    assert_refused(client.maven_put(blobs.maven_path(), b"x"), 403, "SCOPE_MISSING")
    assert as_user(target, target.user("ci")).gradle_get(key).status_code == 404


def test_refused_upload_is_not_stored_or_read(target: Target) -> None:
    """A refused store with Expect: 100-continue is answered before any body is read."""
    client = CacheClient(target, headers={"Expect": "100-continue"})
    response = client.gradle_put(blobs.gradle_key(), blobs.payload(target.max_entry_size // 2))
    assert_refused(response, 401, "CREDENTIALS_MISSING")


@pytest.mark.parametrize("user", ["prometheus", "admin"])
def test_non_cache_users_cannot_touch_the_cache(
    target: Target, stored: tuple[str, str, bytes], user: str
) -> None:
    client = as_user(target, target.user(user))
    assert_refused(client.gradle_get(stored[0]), 403, "SCOPE_MISSING")
    assert_refused(client.gradle_put(blobs.gradle_key(), b"x"), 403, "SCOPE_MISSING")


def test_admin_api_needs_admin_read(target: Target) -> None:
    assert_refused(as_user(target, None).get("/v1/serverStatus"), 401, "CREDENTIALS_MISSING")
    ci = as_user(target, target.user("ci"))
    assert_refused(ci.get("/v1/serverStatus"), 403, "SCOPE_MISSING")


# Malformed and hostile credentials


def _b64(raw: bytes) -> str:
    return base64.b64encode(raw).decode()


MALFORMED = {
    "empty": "",
    "scheme only": "Basic",
    "scheme and space": "Basic ",
    "not base64": "Basic !!!not-base64!!!",
    "no colon": "Basic " + _b64(b"ci"),
    "empty user and token": "Basic " + _b64(b":"),
    "empty token": "Basic " + _b64(b"ci:"),
    "invalid utf-8": "Basic " + _b64(b"ci:\xff\xfe"),
    "bearer empty": "Bearer ",
    "bearer with spaces": "Bearer a b c",
    "unknown scheme": "Digest username=ci",
    "token without scheme": "mmc_e2eCiTokenNotSecretUsedOnlyInTests000001",
    "huge": "Bearer " + "a" * 7000,
}


@pytest.mark.parametrize("header", MALFORMED.values(), ids=MALFORMED.keys())
def test_malformed_authorization_is_401(
    target: Target, stored: tuple[str, str, bytes], header: str
) -> None:
    """Sent raw, since httpx refuses some of these headers (trailing spaces) itself."""
    path = f"/cache/{stored[0]}"
    assert raw.status(str(target.base_url), "GET", path, {"Authorization": header}) == 401
    assert raw.status(str(target.base_url), "PUT", path, {"Authorization": header}, b"x") == 401


def test_scheme_is_case_insensitive(target: Target, stored: tuple[str, str, bytes]) -> None:
    """RFC 9110: the auth scheme is case-insensitive."""
    header = basic(target.user("ci")).replace("Basic", "basic", 1)
    response = CacheClient(target, headers={"Authorization": header}).gradle_get(stored[0])
    assert response.status_code == 200, response.text


def test_colon_in_token_splits_at_the_first_colon(
    target: Target, stored: tuple[str, str, bytes]
) -> None:
    """RFC 7617: the user name ends at the first colon; the rest, colons included, is the token."""
    ci = target.user("ci")
    header = "Basic " + _b64(f"ci:{ci.token.get_secret_value()}:extra".encode())
    response = CacheClient(target, headers={"Authorization": header}).gradle_get(stored[0])
    assert_refused(response, 401, "CREDENTIALS_INVALID")


@settings(
    max_examples=60, deadline=None, suppress_health_check=[HealthCheck.function_scoped_fixture]
)
@given(header=st.text(alphabet=st.characters(min_codepoint=0x20, max_codepoint=0x7E), max_size=200))
def test_fuzzed_authorization_never_errors_or_writes(target: Target, header: str) -> None:
    path = f"/cache/{blobs.gradle_key()}"
    status = raw.status(str(target.base_url), "PUT", path, {"Authorization": header}, b"fuzz")
    assert status in {401, 403}, (header, status)


@pytest.mark.control
def test_restart_keeps_auth(target: Target, controlled: Stack) -> None:
    controlled.restart("mammoth")
    assert as_user(target, None).gradle_get(blobs.gradle_key()).status_code == 401
