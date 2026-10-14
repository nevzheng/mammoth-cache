"""Raw HTTP over a socket, for requests httpx refuses to send (truncated or lying bodies)."""

from __future__ import annotations

import socket
from urllib.parse import urlsplit


def truncated_put(
    base_url: str, path: str, headers: dict[str, str], declared: int, sent: bytes
) -> bytes:
    """PUT declaring `declared` bytes, send only `sent`, then close; return whatever came back."""
    url = urlsplit(base_url)
    host, port = url.hostname or "localhost", url.port or 80
    lines = [f"PUT {path} HTTP/1.1", f"Host: {host}:{port}", f"Content-Length: {declared}"]
    lines += [f"{k}: {v}" for k, v in headers.items()]
    request = ("\r\n".join(lines) + "\r\n\r\n").encode() + sent
    with socket.create_connection((host, port), timeout=5.0) as sock:
        sock.sendall(request)
        sock.shutdown(socket.SHUT_WR)
        chunks: list[bytes] = []
        try:
            while chunk := sock.recv(65536):
                chunks.append(chunk)
        except TimeoutError, ConnectionResetError:
            pass
    return b"".join(chunks)


def status(
    base_url: str, method: str, path: str, headers: dict[str, str], body: bytes = b""
) -> int:
    """Send `path` exactly as given (no dot-segment removal or re-encoding); return the status."""
    url = urlsplit(base_url)
    host, port = url.hostname or "localhost", url.port or 80
    lines = [f"{method} {path} HTTP/1.1", f"Host: {host}:{port}", "Connection: close"]
    lines += [f"Content-Length: {len(body)}", *(f"{k}: {v}" for k, v in headers.items())]
    request = ("\r\n".join(lines) + "\r\n\r\n").encode() + body
    with socket.create_connection((host, port), timeout=5.0) as sock:
        sock.sendall(request)
        head = sock.recv(64)
    return int(head.split(b" ", 2)[1])


def answers_before_body(base_url: str, path: str, headers: dict[str, str], body: bytes) -> bool:
    """PUT sending only half of `body`; whether the node answers before the rest arrives.

    A node that answers a PUT before reading its body (without `Expect: 100-continue`) and then
    closes the connection breaks clients still writing the body: they see a broken pipe.
    """
    url = urlsplit(base_url)
    host, port = url.hostname or "localhost", url.port or 80
    lines = [f"PUT {path} HTTP/1.1", f"Host: {host}:{port}", f"Content-Length: {len(body)}"]
    lines += [f"{k}: {v}" for k, v in headers.items()]
    half = len(body) // 2
    with socket.create_connection((host, port), timeout=5.0) as sock:
        sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode() + body[:half])
        sock.settimeout(0.5)
        try:
            early = sock.recv(64)
        except TimeoutError:
            early = b""
    return bool(early)
