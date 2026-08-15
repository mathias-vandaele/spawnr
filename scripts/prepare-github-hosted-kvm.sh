#!/usr/bin/env bash
set -euo pipefail

if [[ ${RUNNER_OS:-Linux} != Linux ]]; then
  echo 'Spawnr release validation requires a Linux GitHub-hosted runner' >&2
  exit 1
fi

runner_user=$(id -un)

if ! command -v newuidmap >/dev/null || ! command -v newgidmap >/dev/null; then
  sudo apt-get update
  sudo apt-get install --yes --no-install-recommends uidmap
fi

ensure_subordinate_range() {
  local file=$1
  if ! sudo awk -F: -v user="$runner_user" \
    '$1 == user && $3 >= 65536 { found = 1 } END { exit !found }' "$file"; then
    printf '%s:1000000:65536\n' "$runner_user" | sudo tee -a "$file" >/dev/null
  fi
}

ensure_subordinate_range /etc/subuid
ensure_subordinate_range /etc/subgid

if sysctl kernel.unprivileged_userns_clone >/dev/null 2>&1; then
  sudo sysctl -w kernel.unprivileged_userns_clone=1
fi
if sysctl kernel.apparmor_restrict_unprivileged_userns >/dev/null 2>&1; then
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi

if [[ ! -c /dev/kvm ]]; then
  echo 'GitHub-hosted runner does not expose /dev/kvm' >&2
  exit 1
fi
sudo chmod 0666 /dev/kvm

if [[ ! -c /dev/fuse ]]; then
  sudo modprobe fuse 2>/dev/null || true
  sudo mknod -m 0666 /dev/fuse c 10 229
fi
sudo chmod 0666 /dev/fuse

python3 - <<'PY'
import fcntl
import os

kvm = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
try:
    version = fcntl.ioctl(kvm, 0xAE00, 0)
finally:
    os.close(kvm)
if version != 12:
    raise SystemExit(f"unsupported KVM API version: {version}")

fuse = os.open("/dev/fuse", os.O_RDWR | os.O_CLOEXEC)
os.close(fuse)
print("KVM API 12 and FUSE are accessible")
PY

unshare --user --map-auto --map-root-user -- true
echo "subordinate user namespaces are available for $runner_user"
