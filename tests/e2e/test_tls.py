"""Built-in TLS on the node (doc 0014), on the Compose stack's two TLS nodes.

`mammoth-tls` serves a certificate the harness issued from a test CA, as an operator with their own
CA would; tests renew it in place to check the reload. `mammoth-tls-generated` serves a self-signed
certificate it generated itself (`[tls] generate`, issue #87): clients trust the written file once,
so the same certificate has to come back after a restart, and a new one only after the operator
deletes the pair.
"""

from __future__ import annotations

import secrets
import shutil
import ssl
import subprocess
import time
from typing import TYPE_CHECKING

import httpx
import pytest

from mammoth_e2e import tlscerts
from mammoth_e2e.client import basic, sha256
from mammoth_e2e.metrics import parse
from mammoth_e2e.reapi import Reapi
from mammoth_e2e.tlscerts import Authority

if TYPE_CHECKING:
    from collections.abc import Iterator
    from pathlib import Path

    from mammoth_e2e.stack import Stack
    from mammoth_e2e.target import Target, Tls

pytestmark = pytest.mark.requires("tls")

GRADLE = {"Content-Type": "application/vnd.gradle.build-cache-artifact.v2"}
# The node re-reads its certificate files this often (tls::RELOAD_INTERVAL).
RELOAD_INTERVAL = 60.0


@pytest.fixture
def tls(target: Target, live: Stack | None) -> Tls:
    del live
    if target.tls is None:
        pytest.skip(f"target {target.name!r} has no TLS nodes")
    return target.tls


@pytest.fixture
def authority(tls: Tls) -> Authority:
    return Authority(tls.cert_dir)


@pytest.fixture
def base_url(tls: Tls) -> str:
    return str(tls.base_url).rstrip("/")


@pytest.fixture
def https(target: Target, base_url: str, authority: Authority) -> Iterator[httpx.Client]:
    """A client of `mammoth-tls` that trusts only the test CA, as the default user."""
    headers = {}
    if target.credentials is not None:
        headers["Authorization"] = basic(target.credentials)
    with httpx.Client(
        base_url=base_url, verify=authority.context(), headers=headers, timeout=30.0
    ) as client:
        yield client


def _metrics(base_url: str, verify: ssl.SSLContext) -> httpx.Response:
    response = httpx.get(f"{base_url}/metrics", verify=verify, timeout=10.0)
    response.raise_for_status()
    return response


def _connect(base_url: str, context: ssl.SSLContext) -> ssl.SSLSocket:
    return tlscerts.connect(base_url, context)


def test_every_protocol_round_trips_over_tls(https: httpx.Client) -> None:
    key = secrets.token_hex(16)
    body = secrets.token_bytes(64 * 1024)
    assert https.put(f"/cache/{key}", content=body, headers=GRADLE).status_code == 201
    got = https.get(f"/cache/{key}")
    assert (got.status_code, got.content) == (200, body)
    assert got.http_version == "HTTP/1.1"

    path = f"v1/tls/{key}/buildinfo.xml"
    assert https.put(f"/maven/{path}", content=body).status_code == 201
    assert https.get(f"/maven/{path}").content == body

    digest = sha256(body)
    assert https.put(f"/bazel/cas/{digest}", content=body).status_code in {200, 201}
    assert https.get(f"/bazel/cas/{digest}").content == body
    assert https.get("/readyz").status_code == 200


def test_credentials_are_still_checked_over_tls(base_url: str, authority: Authority) -> None:
    with httpx.Client(base_url=base_url, verify=authority.context(), timeout=10.0) as anonymous:
        assert anonymous.get(f"/cache/{secrets.token_hex(16)}").status_code == 401


@pytest.mark.requires("reapi")
def test_grpc_round_trips_over_tls(target: Target, tls: Tls, authority: Authority) -> None:
    if tls.reapi_address is None:
        pytest.skip("the TLS node has no REAPI address in the profile")
    authorization = basic(target.credentials) if target.credentials else None
    client = Reapi(
        tls.reapi_address,
        authorization=authorization,
        root_certificates=authority.ca_cert.read_bytes(),
    )
    try:
        body = secrets.token_bytes(4096)
        digest = client.digest(body)
        assert client.batch_update([(digest, body)]) == [0]
        assert client.batch_read([digest]) == [(0, body)]
    finally:
        client.close()


def test_a_client_that_does_not_trust_the_ca_is_refused(base_url: str) -> None:
    with pytest.raises(httpx.ConnectError, match="CERTIFICATE_VERIFY_FAILED"):
        httpx.get(f"{base_url}/readyz", timeout=10.0)


def test_plain_http_on_the_tls_port_is_refused(base_url: str, authority: Authority) -> None:
    before = parse(_metrics(base_url, authority.context()).text)
    plain = base_url.replace("https://", "http://", 1)
    with pytest.raises(httpx.TransportError):
        httpx.get(f"{plain}/readyz", timeout=10.0)
    after = parse(_metrics(base_url, authority.context()).text)
    failures = "mammoth_tls_handshake_failures_total"
    assert after.value(failures, reason="error") > before.value(failures, reason="error")


@pytest.mark.parametrize("version", [ssl.TLSVersion.TLSv1_2, ssl.TLSVersion.TLSv1_3])
def test_serves_tls_1_2_and_1_3(
    base_url: str, authority: Authority, version: ssl.TLSVersion
) -> None:
    context = authority.context()
    context.minimum_version = version
    context.maximum_version = version
    with _connect(base_url, context) as tls:
        assert tls.version() == version.name.replace("_", ".")


@pytest.mark.filterwarnings("ignore:ssl.TLSVersion:DeprecationWarning")
def test_refuses_tls_1_1_and_older(base_url: str, authority: Authority) -> None:
    context = authority.context()
    # Let the client offer the old versions, so the refusal is the node's.
    context.set_ciphers("DEFAULT:@SECLEVEL=0")
    context.minimum_version = ssl.TLSVersion.TLSv1
    context.maximum_version = ssl.TLSVersion.TLSv1_1
    with pytest.raises(ssl.SSLError):
        _connect(base_url, context).close()


def test_refuses_cbc_suites(base_url: str, authority: Authority) -> None:
    """rustls's defaults have no CBC suites; a TLS 1.2 client offering only those is refused."""
    context = authority.context()
    context.maximum_version = ssl.TLSVersion.TLSv1_2
    context.set_ciphers("ECDHE-ECDSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-ECDSA-AES128-SHA256")
    with pytest.raises(ssl.SSLError):
        _connect(base_url, context).close()


def test_alpn_offers_only_http_1_1(base_url: str, authority: Authority) -> None:
    context = authority.context()
    context.set_alpn_protocols(["h2", "http/1.1"])
    with _connect(base_url, context) as tls:
        assert tls.selected_alpn_protocol() == "http/1.1"
    h2_only = authority.context()
    h2_only.set_alpn_protocols(["h2"])
    with pytest.raises(ssl.SSLError):
        _connect(base_url, h2_only).close()


def test_reports_the_certificate_expiry(base_url: str, authority: Authority) -> None:
    served = tlscerts.served_certificate(base_url, authority.context())
    expiry = parse(_metrics(base_url, authority.context()).text).value(
        "mammoth_tls_certificate_expiry_seconds"
    )
    assert expiry == tlscerts.not_after(tlscerts.der_to_pem(served))


@pytest.mark.control
def test_a_renewed_certificate_is_served_without_a_restart(
    controlled: Stack, base_url: str, authority: Authority
) -> None:
    del controlled
    before = tlscerts.served_certificate(base_url, authority.context())
    reloads = "mammoth_tls_reloads_total"
    reloaded = parse(_metrics(base_url, authority.context()).text).value(reloads, result="ok")

    renewed = authority.issue(days=3)
    renewed_der = tlscerts.cert_der(renewed)
    deadline = time.monotonic() + RELOAD_INTERVAL * 2 + 10
    served = before
    while served != renewed_der:
        if time.monotonic() > deadline:
            pytest.fail("the node still serves the old certificate after two reload intervals")
        time.sleep(2)
        served = tlscerts.served_certificate(base_url, authority.context())

    after = parse(_metrics(base_url, authority.context()).text)
    assert after.value(reloads, result="ok") > reloaded
    assert after.value("mammoth_tls_certificate_expiry_seconds") == tlscerts.not_after(
        tlscerts.der_to_pem(renewed_der)
    )


# The node that generates its own certificate.


@pytest.fixture
def generated(tls: Tls) -> str:
    if tls.generated_base_url is None:
        pytest.skip("the target has no node with [tls] generate")
    return str(tls.generated_base_url).rstrip("/")


def _written(stack: Stack, tls: Tls, tmp_path: Path) -> tuple[bytes, int]:
    """The certificate the generated node wrote to its volume, and its key file's mode."""
    cert, key = tmp_path / "tls.crt", tmp_path / "tls.key"
    cert.unlink(missing_ok=True)
    key.unlink(missing_ok=True)
    stack.copy_out(tls.generated_service, tls.generated_cert, cert)
    stack.copy_out(tls.generated_service, tls.generated_key, key)
    return cert.read_bytes(), key.stat().st_mode & 0o777


@pytest.mark.control
def test_a_generated_certificate_serves_clients_that_trust_the_file(
    controlled: Stack, tls: Tls, generated: str, tmp_path: Path
) -> None:
    pem, key_mode = _written(controlled, tls, tmp_path)
    assert key_mode == 0o600
    served = tlscerts.served_certificate(generated)
    assert tlscerts.pem_to_der(pem) == served, "the node serves a certificate other than the file"

    trust = ssl.create_default_context(cadata=pem.decode())
    key = secrets.token_hex(16)
    body = secrets.token_bytes(4096)
    with httpx.Client(base_url=generated, verify=trust, timeout=10.0) as client:
        assert client.put(f"/cache/{key}", content=body, headers=GRADLE).status_code == 201
        assert client.get(f"/cache/{key}").content == body

    # As the deployment guide says: curl --cacert with the copied file.
    curl = shutil.which("curl")
    if curl is not None:
        cert_file = tmp_path / "tls.crt"
        out = subprocess.run(
            [curl, "--fail", "--silent", "--cacert", str(cert_file), f"{generated}/cache/{key}"],
            capture_output=True,
            check=True,
            timeout=30,
        )
        assert out.stdout == body

    logs = controlled.logs([tls.generated_service], tail=2000)
    assert tlscerts.fingerprint(served) in logs, "startup should log the certificate's fingerprint"
    assert "self-signed" in logs


def test_a_client_that_does_not_trust_the_generated_certificate_is_refused(generated: str) -> None:
    with pytest.raises(httpx.ConnectError, match="CERTIFICATE_VERIFY_FAILED"):
        httpx.get(f"{generated}/readyz", timeout=10.0)


@pytest.mark.control
def test_the_generated_certificate_survives_a_restart(
    controlled: Stack, tls: Tls, generated: str, tmp_path: Path
) -> None:
    pem, _ = _written(controlled, tls, tmp_path)
    served = tlscerts.served_certificate(generated)

    controlled.restart(tls.generated_service)

    assert _written(controlled, tls, tmp_path)[0] == pem, "the certificate file changed"
    assert tlscerts.fingerprint(tlscerts.served_certificate(generated)) == tlscerts.fingerprint(
        served
    )


@pytest.mark.control
def test_deleting_the_generated_pair_makes_a_new_certificate(
    controlled: Stack, tls: Tls, generated: str, tmp_path: Path
) -> None:
    before = tlscerts.served_certificate(generated)

    controlled.stop(tls.generated_service)
    controlled.wipe_disk(tls.generated_service)
    controlled.start(tls.generated_service)

    after = tlscerts.served_certificate(generated)
    assert tlscerts.fingerprint(after) != tlscerts.fingerprint(before)
    assert tlscerts.pem_to_der(_written(controlled, tls, tmp_path)[0]) == after
