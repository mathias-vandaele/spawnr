#!/usr/bin/env python3
"""Validate Spawnr's public release identity and bind artifacts to a tag."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tomllib
from typing import NoReturn


ROOT = pathlib.Path(__file__).resolve().parent.parent
SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
)
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")


def fail(message: str) -> NoReturn:
    raise SystemExit(f"release preflight failed: {message}")


def load_json(path: pathlib.Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read {path}: {error}")
    if not isinstance(value, dict):
        fail(f"{path} is not a JSON object")
    return value


def load_toml(path: pathlib.Path) -> dict:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot read {path}: {error}")


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


def release_identity() -> tuple[dict, str]:
    config = load_toml(ROOT / "release/config.toml")
    workspace = load_toml(ROOT / "Cargo.toml")
    agent = load_toml(ROOT / "crates/spawnr-agent/Cargo.toml")
    protocol = load_toml(ROOT / "crates/spawnr-protocol/Cargo.toml")
    package = workspace.get("workspace", {}).get("package", {})
    cli_version = package.get("version")
    agent_version = agent.get("package", {}).get("version")
    protocol_version = protocol.get("package", {}).get("version")
    cargo_repository = package.get("repository")

    repository = config.get("repository")
    website = config.get("website")
    target = config.get("target")
    runtime_version = config.get("runtime_version")
    minimum = config.get("cli_minimum")
    maximum = config.get("cli_maximum_exclusive")

    if not isinstance(repository, str) or REPOSITORY.fullmatch(repository) is None:
        fail("release/config.toml has an invalid GitHub repository")
    expected_url = f"https://github.com/{repository}"
    if cargo_repository != expected_url:
        fail(
            f"Cargo repository {cargo_repository!r} differs from release repository "
            f"{expected_url!r}"
        )
    if not isinstance(website, str) or re.fullmatch(r"https://[^/]+", website) is None:
        fail("release website must be a bare HTTPS origin")
    site = (ROOT / "site/index.html").read_text(encoding="utf-8")
    if f'href="{expected_url}"' not in site:
        fail("public site source link differs from release/config.toml")
    if f"{website}/install.sh" not in site:
        fail("public site installer URL differs from release/config.toml")
    if target != "x86_64-linux":
        fail(f"V1 release target must be 'x86_64-linux', got {target!r}")
    for label, value in (
        ("CLI", cli_version),
        ("agent", agent_version),
        ("protocol crate", protocol_version),
        ("runtime", runtime_version),
        ("minimum CLI", minimum),
        ("maximum CLI", maximum),
    ):
        if not isinstance(value, str) or SEMVER.fullmatch(value) is None:
            fail(f"{label} version is not canonical SemVer: {value!r}")
    version_key = lambda value: tuple(int(part) for part in value.split("."))
    if not version_key(minimum) <= version_key(cli_version) < version_key(maximum):
        fail("Cargo CLI version is outside the configured runtime compatibility range")
    return config, cli_version


def check_repository(actual: str, expected: str) -> None:
    if actual != expected:
        fail(
            f"workflow/repository identity is {actual!r}, expected {expected!r}; "
            "transfer or rename the repository before publishing"
        )


def origin_repository() -> str:
    result = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        cwd=ROOT,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        fail("cannot resolve the local origin remote")
    remote = result.stdout.strip()
    patterns = (
        r"git@github\.com:([^/]+/[^/]+?)(?:\.git)?$",
        r"https://github\.com/([^/]+/[^/]+?)(?:\.git)?$",
        r"ssh://git@github\.com/([^/]+/[^/]+?)(?:\.git)?$",
    )
    for pattern in patterns:
        match = re.fullmatch(pattern, remote)
        if match is not None:
            return match.group(1)
    fail(f"origin is not a supported GitHub repository URL: {remote!r}")


def check_clean_worktree() -> None:
    result = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=ROOT,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    if result.stdout:
        fail("the Git worktree is not clean")


def check_checksums(artifacts: pathlib.Path) -> None:
    checksums = require_file(artifacts / "SHA256SUMS")
    seen: set[str] = set()
    for number, line in enumerate(checksums.read_text(encoding="utf-8").splitlines(), 1):
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._+-]*)", line)
        if match is None:
            fail(f"invalid SHA256SUMS line {number}")
        expected, name = match.groups()
        if name in seen:
            fail(f"duplicate SHA256SUMS entry {name}")
        seen.add(name)
        asset = require_file(artifacts / name)
        if sha256(asset) != expected:
            fail(f"SHA-256 mismatch for {name}")
    if not seen:
        fail("SHA256SUMS is empty")


def check_runtime_candidate(artifacts: pathlib.Path, candidate_lock: dict) -> None:
    archive = candidate_lock.get("archive")
    if not isinstance(archive, dict):
        fail("candidate runtime lock has no archive object")
    name = archive.get("file_name")
    expected_sha = archive.get("sha256")
    expected_size = archive.get("size_bytes")
    if not isinstance(name, str) or pathlib.PurePath(name).name != name:
        fail("candidate runtime archive has an invalid file name")
    archive_path = require_file(artifacts / name)
    if sha256(archive_path) != expected_sha:
        fail("runtime archive differs from the candidate lock digest")
    if archive_path.stat().st_size != expected_size:
        fail("runtime archive differs from the candidate lock size")


def check_cli_candidate(
    artifacts: pathlib.Path, candidate_lock: dict, cli_version: str, repository: str
) -> None:
    cli = require_file(artifacts / f"spawnr-{cli_version}-x86_64-linux")
    require_file(artifacts / f"spawnr_{cli_version}-1_amd64.deb")
    require_file(artifacts / f"spawnr-{cli_version}-1.x86_64.rpm")
    license_file = require_file(artifacts / "LICENSE")
    pkgbuild = require_file(artifacts / "PKGBUILD").read_text(encoding="utf-8")
    srcinfo = require_file(artifacts / "spawnr-bin.SRCINFO").read_text(
        encoding="utf-8"
    )
    installer = require_file(artifacts / "install.sh").read_text(encoding="utf-8")
    if f"pkgver={cli_version}\n" not in pkgbuild:
        fail("AUR PKGBUILD version differs from the CLI tag")
    expected_url = (
        f"https://github.com/{repository}/releases/download/v{cli_version}/"
        f"spawnr-{cli_version}-x86_64-linux"
    )
    if expected_url not in srcinfo:
        fail("AUR .SRCINFO does not use the versioned CLI release asset")
    if f"sha256sums_x86_64 = {sha256(cli)}" not in srcinfo:
        fail("AUR .SRCINFO is not pinned to the candidate CLI digest")
    if f"sha256sums_x86_64 = {sha256(license_file)}" not in srcinfo:
        fail("AUR .SRCINFO is not pinned to the candidate licence digest")
    for declaration in (
        f"version='{cli_version}'",
        f"repository='{repository}'",
        f"cli_sha256='{sha256(cli)}'",
        f"cli_size_bytes='{cli.stat().st_size}'",
    ):
        if declaration not in installer:
            fail(f"installer does not pin {declaration}")

    committed_lock_path = ROOT / "release/runtime.lock.json"
    if not committed_lock_path.is_file():
        fail(
            "CLI releases require release/runtime.lock.json; promote the independently "
            "reproduced runtime lock first"
        )
    if load_json(committed_lock_path) != candidate_lock:
        fail("committed runtime lock differs from the reproduced candidate lock")


def check_candidate(tag: str, artifacts: pathlib.Path, config: dict, cli_version: str) -> None:
    if not artifacts.is_dir():
        fail(f"artifact directory does not exist: {artifacts}")
    check_checksums(artifacts)
    candidate_lock = load_json(require_file(artifacts / "runtime.lock.json"))
    runtime_version = candidate_lock.get("runtime_version")
    release_tag = candidate_lock.get("release_tag")
    if runtime_version != config["runtime_version"]:
        fail("candidate runtime version differs from release/config.toml")
    if candidate_lock.get("target") != config["target"]:
        fail("candidate target differs from release/config.toml")
    expected_compatibility = {
        "minimum": config["cli_minimum"],
        "maximum_exclusive": config["cli_maximum_exclusive"],
    }
    if candidate_lock.get("cli_compatibility") != expected_compatibility:
        fail("candidate CLI compatibility differs from release/config.toml")
    if release_tag != f"runtime-v{runtime_version}":
        fail("candidate runtime release tag is inconsistent")
    expected_prefix = (
        f"https://github.com/{config['repository']}/releases/download/{release_tag}/"
    )
    archive = candidate_lock.get("archive")
    if not isinstance(archive, dict) or not str(archive.get("url", "")).startswith(
        expected_prefix
    ):
        fail("candidate runtime URL differs from the public release repository")

    if tag.startswith("runtime-v"):
        if tag != release_tag:
            fail(f"tag {tag!r} does not match runtime lock {release_tag!r}")
        check_runtime_candidate(artifacts, candidate_lock)
        return

    if tag != f"v{cli_version}":
        fail(f"tag {tag!r} does not match Cargo version v{cli_version}")
    check_cli_candidate(artifacts, candidate_lock, cli_version, config["repository"])


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    repository = subparsers.add_parser("repository")
    repository.add_argument("actual")
    candidate = subparsers.add_parser("candidate")
    candidate.add_argument("tag")
    candidate.add_argument("artifacts", type=pathlib.Path)
    candidate.add_argument("--repository")
    candidate.add_argument("--check-remote", action="store_true")
    candidate.add_argument("--require-clean", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    config, cli_version = release_identity()
    expected_repository = config["repository"]
    if args.command == "repository":
        check_repository(args.actual, expected_repository)
        print(f"release repository verified: {expected_repository}")
        return
    if args.repository is not None:
        check_repository(args.repository, expected_repository)
    if args.check_remote:
        check_repository(origin_repository(), expected_repository)
    if args.require_clean:
        check_clean_worktree()
    check_candidate(args.tag, args.artifacts.resolve(), config, cli_version)
    print(f"release candidate verified for {args.tag} in {expected_repository}")


if __name__ == "__main__":
    main()
