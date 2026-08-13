#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo 'usage: check-release-relocation.sh <static-spawnr> <runtime.tar.zst>' >&2
  exit 2
fi

static_cli=$1
runtime_archive=$2
for tool in bwrap jq readelf sha256sum stat tar; do
  command -v "$tool" >/dev/null || {
    echo "release relocation check requires $tool" >&2
    exit 1
  }
done

test_root=$(mktemp -d /tmp/spawnr-release-relocation.XXXXXX)
trap 'rm -rf -- "$test_root"' EXIT
tar --extract --file="$runtime_archive" --directory="$test_root"
install -m0555 "$static_cli" "$test_root/spawnr"
install -d -m0755 "$test_root/dev" "$test_root/etc" "$test_root/proc" "$test_root/tmp"

while IFS=$'\t' read -r path size digest executable; do
  candidate="$test_root/$path"
  [[ -f "$candidate" && ! -L "$candidate" ]]
  [[ "$(stat --format=%s "$candidate")" == "$size" ]]
  [[ "$(sha256sum "$candidate" | cut --delimiter=' ' --fields=1)" == "$digest" ]]
  if [[ "$executable" == true ]]; then
    [[ -x "$candidate" ]]
  else
    [[ ! -x "$candidate" ]]
  fi
done < <(jq -r '.files[] | [.path, .size_bytes, .sha256, .executable] | @tsv' "$test_root/manifest.json")

for binary in "$test_root/spawnr" "$test_root"/bin/* "$test_root/guest/busybox" "$test_root/guest/spawnr-agent"; do
  if readelf --program-headers "$binary" | grep -q 'Requesting program interpreter'; then
    echo "release executable is dynamically linked: $binary" >&2
    exit 1
  fi
done

sandbox=(
  bwrap
  --unshare-all
  --die-with-parent
  --uid 0
  --gid 0
  --ro-bind "$test_root" /
  --proc /proc
  --dev /dev
)

"${sandbox[@]}" /spawnr --version
"${sandbox[@]}" /bin/cloud-hypervisor --version
"${sandbox[@]}" /bin/passt --version
"${sandbox[@]}" /bin/skopeo --version
"${sandbox[@]}" /bin/umoci --version
"${sandbox[@]}" /bin/unshare --version
"${sandbox[@]}" /guest/spawnr-agent --help >/dev/null

"${sandbox[@]}" --tmpfs /tmp /guest/busybox sh -c '
  /guest/busybox truncate -s 64M /tmp/environment.raw
  /bin/mkfs.ext4 -q -F /tmp/environment.raw
  /bin/e2fsck -fn /tmp/environment.raw >/dev/null
'

if daemon_error=$("${sandbox[@]}" /bin/skopeo inspect docker-daemon:forbidden 2>&1); then
  echo 'release skopeo unexpectedly supports the Docker daemon transport' >&2
  exit 1
fi
grep -q 'not supported in this build' <<<"$daemon_error"

echo 'release CLI and runtime are static, internally consistent, and relocatable'
