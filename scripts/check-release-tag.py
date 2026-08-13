#!/usr/bin/env python3
"""Bind a generated Spawnr release candidate to its immutable Git tag."""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import sys
import tomllib


def fail(message: str) -> "NoReturn":
    raise SystemExit(f"release tag check failed: {message}")


def load_json(path: pathlib.Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read {path}: {error}")
    if not isinstance(value, dict):
        fail(f"{path} is not a JSON object")
    return value


def require_file(path: pathlib.Path) -> pathlib.Path:
    if not path.is_file():
        fail(f"candidate has no required asset {path.name}")
    return path


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: check-release-tag.py TAG ARTIFACT_DIRECTORY")
    tag = sys.argv[1]
    artifacts = pathlib.Path(sys.argv[2]).resolve()
    if not artifacts.is_dir():
        fail(f"artifact directory does not exist: {artifacts}")

    candidate_lock_path = artifacts / "runtime.lock.json"
    candidate_lock = load_json(candidate_lock_path)
    runtime_version = candidate_lock.get("runtime_version")
    release_tag = candidate_lock.get("release_tag")
    if not isinstance(runtime_version, str) or not isinstance(release_tag, str):
        fail("candidate runtime lock has no version or release tag")
    if release_tag != f"runtime-v{runtime_version}":
        fail("candidate runtime release tag is inconsistent")

    if tag.startswith("runtime-v"):
        if tag != release_tag:
            fail(f"tag {tag!r} does not match runtime lock {release_tag!r}")
        return

    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag)
    if match is None:
        fail(f"unsupported tag {tag!r}")
    cli_version = tag[1:]
    workspace = tomllib.loads(pathlib.Path("Cargo.toml").read_text(encoding="utf-8"))
    declared_version = workspace.get("workspace", {}).get("package", {}).get("version")
    if declared_version != cli_version:
        fail(f"tag version {cli_version} differs from Cargo version {declared_version}")
    cli = require_file(artifacts / f"spawnr-{cli_version}-x86_64-linux")
    require_file(artifacts / f"spawnr_{cli_version}-1_amd64.deb")
    require_file(artifacts / f"spawnr-{cli_version}-1.x86_64.rpm")
    license_file = require_file(artifacts / "LICENSE")
    pkgbuild = require_file(artifacts / "PKGBUILD").read_text(encoding="utf-8")
    srcinfo = require_file(artifacts / "spawnr-bin.SRCINFO").read_text(
        encoding="utf-8"
    )
    if f"pkgver={cli_version}\n" not in pkgbuild:
        fail("AUR PKGBUILD version differs from the CLI tag")
    expected_url = (
        f"https://github.com/spawnr-dev/spawnr/releases/download/v{cli_version}/"
        f"spawnr-{cli_version}-x86_64-linux"
    )
    if expected_url not in srcinfo:
        fail("AUR .SRCINFO does not use the versioned CLI release asset")
    if f"sha256sums_x86_64 = {sha256(cli)}" not in srcinfo:
        fail("AUR .SRCINFO is not pinned to the candidate CLI digest")
    if f"sha256sums_x86_64 = {sha256(license_file)}" not in srcinfo:
        fail("AUR .SRCINFO is not pinned to the candidate licence digest")

    committed_lock_path = pathlib.Path("release/runtime.lock.json")
    if not committed_lock_path.is_file():
        fail(
            "CLI releases require release/runtime.lock.json; promote the independently "
            "reproduced runtime lock first"
        )
    committed_lock = load_json(committed_lock_path)
    if committed_lock != candidate_lock:
        fail("committed runtime lock differs from the reproduced candidate lock")


if __name__ == "__main__":
    main()
