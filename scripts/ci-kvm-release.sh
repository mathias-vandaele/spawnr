#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo 'usage: ci-kvm-release.sh ARTIFACT_DIRECTORY' >&2
  exit 2
fi
: "${SPAWNR_E2E_ENVIRONMENT:?set a digest-pinned OCI development environment}"

artifacts=$(realpath -- "$1")
(cd "$artifacts" && sha256sum --check SHA256SUMS)
cli_version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
runtime_version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["runtime_version"])' "$artifacts/runtime.lock.json")
spawnr_bin="$artifacts/spawnr-$cli_version-x86_64-linux"
runtime_archive="$artifacts/spawnr-runtime-$runtime_version-x86_64-linux.tar.zst"
if [[ ! -x "$spawnr_bin" ]]; then
  chmod 0555 "$spawnr_bin"
fi

test -c /dev/kvm
test -r /dev/kvm
test -w /dev/kvm
test -c /dev/fuse

test_root=$(mktemp -d /tmp/spawnr-release-kvm.XXXXXX)
cleanup() {
  if [[ -d "$test_root" ]]; then
    chmod -R u+w "$test_root" 2>/dev/null || true
    find "$test_root" -depth -delete 2>/dev/null || true
  fi
}
trap cleanup EXIT

export SPAWNR_HOME="$test_root/data"
export SPAWNR_BIN="$spawnr_bin"
"$spawnr_bin" setup \
  --runtime-lock "$artifacts/runtime.lock.json" \
  --runtime-archive "$runtime_archive"
"$spawnr_bin" doctor

runtime="$SPAWNR_HOME/runtime/$runtime_version"
export SPAWNR_UMOCI="$runtime/bin/umoci"
export SPAWNR_UNSHARE="$runtime/bin/unshare"
"$SPAWNR_UNSHARE" --user --map-auto --map-root-user -- true

scripts/e2e-critical.sh
