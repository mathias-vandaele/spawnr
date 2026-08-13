#!/usr/bin/env python3
"""Run one non-TTY command through Spawnr's private test control socket.

This is deliberately development tooling, not a user-facing runtime path. It
speaks the same Cloud Hypervisor hybrid-vsock handshake and framed protocol as
the Rust host client so shell E2E tests can make deterministic assertions.
"""

import argparse
import json
import socket
import struct
import sys


def read_exact(connection: socket.socket, count: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < count:
        chunk = connection.recv(count - len(chunks))
        if not chunk:
            raise RuntimeError("guest control connection closed early")
        chunks.extend(chunk)
    return bytes(chunks)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    parser.add_argument("--cwd", default="/workspace")
    parser.add_argument("argv", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    if not arguments.argv:
        parser.error("a guest command is required")

    request = {
        "op": "exec",
        "argv": arguments.argv,
        "cwd": arguments.cwd,
        "env": {},
        "tty": False,
        "rows": 0,
        "cols": 0,
    }
    payload = json.dumps(request, separators=(",", ":")).encode()

    with socket.socket(socket.AF_UNIX) as connection:
        connection.connect(arguments.socket)
        connection.sendall(b"CONNECT 19870\n")
        handshake = bytearray()
        while not handshake.endswith(b"\n"):
            handshake.extend(read_exact(connection, 1))
            if len(handshake) > 128:
                raise RuntimeError("oversized hybrid-vsock handshake")
        if not handshake.startswith(b"OK "):
            raise RuntimeError(f"hybrid-vsock handshake failed: {handshake!r}")
        connection.sendall(struct.pack(">I", len(payload)) + payload)
        length = struct.unpack(">I", read_exact(connection, 4))[0]
        if length > 8 * 1024 * 1024:
            raise RuntimeError("oversized guest response")
        response = json.loads(read_exact(connection, length))

    if response.get("kind") == "command_result":
        sys.stdout.write(response.get("stdout", ""))
        sys.stderr.write(response.get("stderr", ""))
        return int(response.get("exit_code", 1))
    raise RuntimeError(f"unexpected guest response: {response!r}")


if __name__ == "__main__":
    raise SystemExit(main())
