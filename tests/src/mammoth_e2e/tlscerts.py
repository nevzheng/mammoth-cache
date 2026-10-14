"""Test certificates for the built-in TLS nodes (doc 0014).

The harness makes a throwaway CA when the stack starts and issues the node's certificate from it,
so nothing is checked in that expires. Tests issue renewals from the same CA to exercise reload.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import ipaddress
import socket
import ssl
from dataclasses import dataclass
from typing import TYPE_CHECKING

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

if TYPE_CHECKING:
    from pathlib import Path

    from cryptography.hazmat.primitives.asymmetric.ec import EllipticCurvePrivateKey

# What clients connect to: the Compose ports are published on the host.
DNS_NAME = "localhost"
IP_ADDRESS = "127.0.0.1"


def _name(common_name: str) -> x509.Name:
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])


def _pem_key(key: EllipticCurvePrivateKey) -> bytes:
    return key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    )


def _write(path: Path, data: bytes) -> None:
    """Replace a file in one step, as cert-manager and certbot do, so the node never reads half
    a file. Readable by every user: the node runs as another uid (65532) than the harness."""
    temp = path.with_name(path.name + ".tmp")
    temp.write_bytes(data)
    temp.chmod(0o644)
    temp.replace(path)


@dataclass(frozen=True, slots=True)
class Authority:
    """A test CA in `dir`: `ca.crt` and `ca.key`, and the leaf it last issued in `tls.crt` and
    `tls.key`."""

    dir: Path

    @property
    def ca_cert(self) -> Path:
        return self.dir / "ca.crt"

    @property
    def leaf_cert(self) -> Path:
        return self.dir / "tls.crt"

    @property
    def leaf_key(self) -> Path:
        return self.dir / "tls.key"

    @classmethod
    def ensure(cls, directory: Path) -> tuple[Authority, bool]:
        """The CA in `directory`, or a new one if there is none or its leaf expires within a
        day. Returns it and whether it is new."""
        authority = cls(directory)
        try:
            pem = authority.leaf_cert.read_bytes()
            authority.ca_cert.stat()
            (directory / "ca.key").stat()
        except FileNotFoundError:
            return cls.create(directory), True
        if not_after(pem) - dt.datetime.now(dt.UTC).timestamp() < 86_400:
            return cls.create(directory), True
        return authority, False

    @classmethod
    def create(cls, directory: Path) -> Authority:
        """A new CA, and a first leaf, replacing whatever `directory` held."""
        directory.mkdir(parents=True, exist_ok=True)
        directory.chmod(0o755)
        key = ec.generate_private_key(ec.SECP256R1())
        now = dt.datetime.now(dt.UTC)
        cert = (
            x509.CertificateBuilder()
            .subject_name(_name("mammoth-e2e test CA"))
            .issuer_name(_name("mammoth-e2e test CA"))
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - dt.timedelta(hours=1))
            .not_valid_after(now + dt.timedelta(days=60))
            .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
            .add_extension(
                x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False
            )
            .add_extension(
                x509.KeyUsage(
                    digital_signature=False,
                    content_commitment=False,
                    key_encipherment=False,
                    data_encipherment=False,
                    key_agreement=False,
                    key_cert_sign=True,
                    crl_sign=True,
                    encipher_only=False,
                    decipher_only=False,
                ),
                critical=True,
            )
            .sign(key, hashes.SHA256())
        )
        _write(directory / "ca.key", _pem_key(key))
        _write(directory / "ca.crt", cert.public_bytes(serialization.Encoding.PEM))
        authority = cls(directory)
        authority.issue()
        return authority

    def issue(self, *, days: int = 30) -> x509.Certificate:
        """Issue a new leaf for `localhost` and `127.0.0.1` into `tls.crt` and `tls.key`."""
        ca_key = serialization.load_pem_private_key((self.dir / "ca.key").read_bytes(), None)
        if not isinstance(ca_key, ec.EllipticCurvePrivateKey):
            raise TypeError("the test CA key is not an EC key")
        ca = x509.load_pem_x509_certificate(self.ca_cert.read_bytes())
        key = ec.generate_private_key(ec.SECP256R1())
        now = dt.datetime.now(dt.UTC).replace(microsecond=0)
        sans: list[x509.GeneralName] = [
            x509.DNSName(DNS_NAME),
            x509.IPAddress(ipaddress.ip_address(IP_ADDRESS)),
        ]
        cert = (
            x509.CertificateBuilder()
            .subject_name(_name(DNS_NAME))
            .issuer_name(ca.subject)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - dt.timedelta(hours=1))
            .not_valid_after(now + dt.timedelta(days=days))
            .add_extension(x509.SubjectAlternativeName(sans), critical=False)
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.ExtendedKeyUsage([x509.OID_SERVER_AUTH]), critical=False)
            # Python's default context checks X.509 strictly and wants both key identifiers.
            .add_extension(
                x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False
            )
            .add_extension(
                x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()),
                critical=False,
            )
            .sign(ca_key, hashes.SHA256())
        )
        # A renewal that writes the key before the certificate: the node refuses the mismatched
        # pair until the certificate follows, and keeps serving the old one meanwhile.
        _write(self.leaf_key, _pem_key(key))
        _write(self.leaf_cert, cert.public_bytes(serialization.Encoding.PEM))
        return cert

    def context(self) -> ssl.SSLContext:
        """A client context that trusts only this CA."""
        return ssl.create_default_context(cafile=self.ca_cert)


def connect(base_url: str, context: ssl.SSLContext) -> ssl.SSLSocket:
    """A TLS connection to `https://host:port`, handshake done."""
    host, port = base_url.removeprefix("https://").rstrip("/").rsplit(":", 1)
    tcp = socket.create_connection((host, int(port)), timeout=10)
    try:
        return context.wrap_socket(tcp, server_hostname=host)
    except BaseException:
        tcp.close()
        raise


def served_certificate(base_url: str, context: ssl.SSLContext | None = None) -> bytes:
    """The DER certificate a TLS node serves at `https://host:port`. Without `context`, nothing
    is verified."""
    if context is None:
        context = ssl.create_default_context()
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
    with connect(base_url, context) as tls:
        der = tls.getpeercert(binary_form=True)
    if der is None:
        raise ssl.SSLError("the node sent no certificate")
    return der


def fingerprint(der: bytes) -> str:
    """SHA-256 as colon-separated upper-case hex, the form `keytool` and the node's log print."""
    return ":".join(f"{b:02X}" for b in hashlib.sha256(der).digest())


def cert_der(cert: x509.Certificate) -> bytes:
    return cert.public_bytes(serialization.Encoding.DER)


def der_to_pem(der: bytes) -> bytes:
    return x509.load_der_x509_certificate(der).public_bytes(serialization.Encoding.PEM)


def pem_to_der(pem: bytes) -> bytes:
    return x509.load_pem_x509_certificate(pem).public_bytes(serialization.Encoding.DER)


def not_after(pem: bytes) -> int:
    """The certificate's expiry, in Unix seconds."""
    return int(x509.load_pem_x509_certificate(pem).not_valid_after_utc.timestamp())
