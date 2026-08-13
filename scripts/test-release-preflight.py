#!/usr/bin/env python3
"""Behavioral checks for the public release preflight."""

from __future__ import annotations

import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
PREFLIGHT = ROOT / "scripts/release-preflight.py"


def run(*arguments: str, success: bool) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [sys.executable, str(PREFLIGHT), *arguments],
        cwd=ROOT,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if (result.returncode == 0) != success:
        raise AssertionError(
            f"unexpected preflight status {result.returncode}: {result.stdout}{result.stderr}"
        )
    return result


def write_runtime_candidate(directory: pathlib.Path) -> None:
    archive = directory / "spawnr-runtime-0.1.0-x86_64-linux.tar.zst"
    archive.write_bytes(b"deterministic runtime fixture\n")
    lock = {
        "runtime_version": "0.1.0",
        "target": "x86_64-linux",
        "cli_compatibility": {
            "minimum": "0.1.0",
            "maximum_exclusive": "0.2.0",
        },
        "release_tag": "runtime-v0.1.0",
        "archive": {
            "file_name": archive.name,
            "url": (
                "https://github.com/spawnr-dev/spawnr/releases/download/"
                f"runtime-v0.1.0/{archive.name}"
            ),
            "size_bytes": archive.stat().st_size,
            "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
        },
    }
    lock_path = directory / "runtime.lock.json"
    lock_path.write_text(json.dumps(lock), encoding="utf-8")
    lines = []
    for path in (lock_path, archive):
        lines.append(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
    (directory / "SHA256SUMS").write_text("".join(lines), encoding="utf-8")


def main() -> None:
    run("repository", "spawnr-dev/spawnr", success=True)
    failure = run("repository", "someone/fork", success=False)
    if "expected 'spawnr-dev/spawnr'" not in failure.stderr:
        raise AssertionError("repository mismatch did not identify the expected repository")
    with tempfile.TemporaryDirectory(prefix="spawnr-preflight-test-") as temporary:
        candidate = pathlib.Path(temporary)
        write_runtime_candidate(candidate)
        run("candidate", "runtime-v0.1.0", str(candidate), success=True)
        run("candidate", "runtime-v0.1.1", str(candidate), success=False)
        archive = candidate / "spawnr-runtime-0.1.0-x86_64-linux.tar.zst"
        archive.write_bytes(b"tampered\n")
        run("candidate", "runtime-v0.1.0", str(candidate), success=False)


if __name__ == "__main__":
    main()
