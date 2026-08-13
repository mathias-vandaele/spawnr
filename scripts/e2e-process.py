#!/usr/bin/env python3
"""Identity-safe process operations for Spawnr's Linux end-to-end tests."""

import argparse
import errno
import json
import os
import pathlib
import signal
import sys
import time


def read_identity(path: pathlib.Path) -> dict[str, object]:
    identity = json.loads(path.read_text())
    required = {
        "pid",
        "boot_id",
        "start_time_ticks",
        "executable_device",
        "executable_inode",
    }
    if not required.issubset(identity):
        raise RuntimeError(f"incomplete process identity in {path}")
    if not isinstance(identity["pid"], int) or identity["pid"] <= 0:
        raise RuntimeError(f"invalid process PID in {path}")
    return identity


def proc_state_and_start(pid: int) -> tuple[str, int]:
    stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    close = stat.rfind(")")
    if close < 0:
        raise RuntimeError(f"malformed /proc/{pid}/stat")
    fields = stat[close + 1 :].split()
    if len(fields) < 20:
        raise RuntimeError(f"short /proc/{pid}/stat")
    return fields[0], int(fields[19])


def identity_is_live(identity: dict[str, object]) -> bool:
    pid = int(identity["pid"])
    try:
        state, start = proc_state_and_start(pid)
    except FileNotFoundError:
        return False
    boot_id = pathlib.Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    if boot_id != identity["boot_id"] or start != identity["start_time_ticks"]:
        return False
    if state in {"Z", "X"}:
        return False
    try:
        executable = os.stat(f"/proc/{pid}/exe")
    except PermissionError:
        # Non-dumpable helpers may hide exe; boot ID plus start time remains a
        # non-reusable identity, matching Spawnr's runtime policy.
        return True
    except FileNotFoundError:
        return False
    return (
        executable.st_dev == identity["executable_device"]
        and executable.st_ino == identity["executable_inode"]
    )


def signal_identity(path: pathlib.Path, name: str) -> None:
    identity = read_identity(path)
    if not identity_is_live(identity):
        raise RuntimeError(f"recorded process in {path} is not live")
    pidfd = os.pidfd_open(int(identity["pid"]))
    try:
        # Revalidate after opening pidfd so PID exit/reuse cannot redirect the
        # signal to an unrelated process.
        if not identity_is_live(identity):
            raise RuntimeError(f"recorded process in {path} changed identity")
        signum = {"TERM": signal.SIGTERM, "KILL": signal.SIGKILL}[name]
        try:
            signal.pidfd_send_signal(pidfd, signum)
        except ProcessLookupError:
            pass
    finally:
        os.close(pidfd)


def wait_dead(path: pathlib.Path, timeout: float) -> None:
    identity = read_identity(path)
    deadline = time.monotonic() + timeout
    while identity_is_live(identity):
        if time.monotonic() >= deadline:
            raise RuntimeError(
                f"owned process {identity['pid']} from {path} is still live"
            )
        time.sleep(0.05)


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    send = subparsers.add_parser("signal")
    send.add_argument("identity", type=pathlib.Path)
    send.add_argument("signal", choices=("TERM", "KILL"))
    dead = subparsers.add_parser("wait-dead")
    dead.add_argument("identity", type=pathlib.Path)
    dead.add_argument("--timeout", type=float, default=5.0)
    arguments = parser.parse_args()
    try:
        if arguments.command == "signal":
            signal_identity(arguments.identity, arguments.signal)
        else:
            wait_dead(arguments.identity, arguments.timeout)
    except (KeyError, OSError, RuntimeError, ValueError) as error:
        if isinstance(error, OSError) and error.errno == errno.ESRCH:
            return 0
        print(f"e2e-process: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
