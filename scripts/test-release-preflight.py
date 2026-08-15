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


def write_release_candidate(directory: pathlib.Path) -> None:
    archive = directory / "spawnr-runtime-0.1.0-x86_64-linux.tar.zst"
    archive.write_bytes(b"deterministic runtime fixture\n")
    cli = directory / "spawnr-0.1.0-x86_64-linux"
    cli.write_bytes(b"deterministic CLI fixture\n")
    license_file = directory / "LICENSE"
    license_file.write_text("fixture license\n", encoding="utf-8")
    (directory / "spawnr_0.1.0-1_amd64.deb").write_bytes(b"deb fixture\n")
    (directory / "spawnr-0.1.0-1.x86_64.rpm").write_bytes(b"rpm fixture\n")
    (directory / "PKGBUILD").write_text("pkgver=0.1.0\n", encoding="utf-8")
    cli_url = (
        "https://github.com/mathias-vandaele/spawnr/releases/download/v0.1.0/"
        f"{cli.name}"
    )
    (directory / "spawnr-bin.SRCINFO").write_text(
        f"source_x86_64 = {cli_url}\n"
        f"sha256sums_x86_64 = {hashlib.sha256(cli.read_bytes()).hexdigest()}\n"
        f"sha256sums_x86_64 = {hashlib.sha256(license_file.read_bytes()).hexdigest()}\n",
        encoding="utf-8",
    )
    (directory / "install.sh").write_text(
        "version='0.1.0'\n"
        "repository='mathias-vandaele/spawnr'\n"
        f"cli_sha256='{hashlib.sha256(cli.read_bytes()).hexdigest()}'\n"
        f"cli_size_bytes='{cli.stat().st_size}'\n",
        encoding="utf-8",
    )
    lock = {
        "runtime_version": "0.1.0",
        "target": "x86_64-linux",
        "cli_compatibility": {
            "minimum": "0.1.0",
            "maximum_exclusive": "0.2.0",
        },
        "release_tag": "v0.1.0",
        "archive": {
            "file_name": archive.name,
            "url": (
                "https://github.com/mathias-vandaele/spawnr/releases/download/"
                f"v0.1.0/{archive.name}"
            ),
            "size_bytes": archive.stat().st_size,
            "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
        },
    }
    lock_path = directory / "runtime.lock.json"
    lock_path.write_text(json.dumps(lock), encoding="utf-8")
    lines = []
    for path in sorted(directory.iterdir()):
        if path.name == "SHA256SUMS":
            continue
        lines.append(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
    (directory / "SHA256SUMS").write_text("".join(lines), encoding="utf-8")


def main() -> None:
    run("repository", "mathias-vandaele/spawnr", success=True)
    failure = run("repository", "someone/fork", success=False)
    if "expected 'mathias-vandaele/spawnr'" not in failure.stderr:
        raise AssertionError("repository mismatch did not identify the expected repository")
    with tempfile.TemporaryDirectory(prefix="spawnr-preflight-test-") as temporary:
        candidate = pathlib.Path(temporary)
        write_release_candidate(candidate)
        run("candidate", "v0.1.0", str(candidate), success=True)
        run("candidate", "v0.1.1", str(candidate), success=False)
        archive = candidate / "spawnr-runtime-0.1.0-x86_64-linux.tar.zst"
        archive.write_bytes(b"tampered\n")
        run("candidate", "v0.1.0", str(candidate), success=False)


if __name__ == "__main__":
    main()
