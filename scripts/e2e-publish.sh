#!/usr/bin/env bash
set -euo pipefail

: "${SPAWNR_HOME:?set SPAWNR_HOME to a disposable, fully installed Spawnr data root}"

spawnr_bin=${SPAWNR_BIN:-spawnr}
control=${SPAWNR_E2E_CONTROL:-"$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/e2e-control.py"}
source_ref=${SPAWNR_E2E_ENVIRONMENT:-docker.io/library/ubuntu:24.04}
umoci_bin=${SPAWNR_UMOCI:-umoci}
unshare_bin=${SPAWNR_UNSHARE:-unshare}
environment_sentinel=SPAWNR_ENVIRONMENT_SENTINEL_81A20F35
workspace_sentinel=SPAWNR_WORKSPACE_SECRET_98F31D77
session_sentinel=SPAWNR_SESSION_SECRET_4C67A6D2
original=spawnr-e2e-original
fresh=spawnr-e2e-fresh
layout=$(mktemp -d /tmp/spawnr-published.XXXXXX)
offline_bundle="${layout}.offline-bundle"

machine_socket() {
  local name=$1
  local machine_id
  machine_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["machines"][sys.argv[2]]["id"])' \
    "$SPAWNR_HOME/state.json" "$name")
  printf '%s/machines/%s/session/vsock.sock\n' "$SPAWNR_HOME" "$machine_id"
}

cleanup() {
  "$spawnr_bin" rm "$original" --force >/dev/null 2>&1 || true
  "$spawnr_bin" rm "$fresh" --force >/dev/null 2>&1 || true
  if [[ -e "$offline_bundle" ]]; then
    "$unshare_bin" --user --map-auto --map-root-user --mount --fork -- /bin/rm -rf -- "$offline_bundle" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$layout"
}
trap cleanup EXIT

"$spawnr_bin" init "$original" --environment "$source_ref"
export GH_TOKEN=$session_sentinel
"$spawnr_bin" start "$original"
original_socket=$(machine_socket "$original")
"$control" --socket "$original_socket" sudo /bin/sh -c \
  'apt-get update >/tmp/spawnr-apt.log 2>&1 && DEBIAN_FRONTEND=noninteractive apt-get install -y git openssh-client ca-certificates >>/tmp/spawnr-apt.log 2>&1 && rm -f /tmp/spawnr-apt.log'
"$control" --socket "$original_socket" sudo /bin/sh -c \
  "printf %s $environment_sentinel >/opt/spawnr-environment-sentinel"
"$control" --socket "$original_socket" /bin/sh -c \
  "printf %s $workspace_sentinel >/workspace/private-sentinel"

"$spawnr_bin" stop "$original"
"$spawnr_bin" start "$original"
original_socket=$(machine_socket "$original")
"$control" --socket "$original_socket" /bin/sh -c \
  'test -f /opt/spawnr-environment-sentinel && test -f /workspace/private-sentinel'

"$spawnr_bin" publish "$original" "oci:$layout:v2"

# Inspect the artifact before boot mounts can hide the environment's underlying
# /workspace or /run paths. This is the authoritative leak assertion.
"$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
  "$umoci_bin" unpack --image "$layout:v2" "$offline_bundle"
"$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
  /bin/sh -eu -c '
    root=$1
    environment=$2
    workspace=$3
    session=$4
    test "$(cat "$root/opt/spawnr-environment-sentinel")" = "$environment"
    for secret in "$workspace" "$session"; do
      if find "$root" -xdev -type f -size -8M -exec grep -lF "$secret" {} + 2>/dev/null | grep -q .; then
        echo "secret leaked into published environment: $secret" >&2
        exit 1
      fi
    done
    test ! -e "$root/run/spawnr/gh-token"
  ' spawnr-offline-check "$offline_bundle/rootfs" \
  "$environment_sentinel" "$workspace_sentinel" "$session_sentinel"

unset GH_TOKEN GITHUB_TOKEN SSH_AUTH_SOCK
"$spawnr_bin" init "$fresh" --environment "oci:$layout:v2"
"$spawnr_bin" start "$fresh"
fresh_socket=$(machine_socket "$fresh")
"$control" --socket "$fresh_socket" /bin/sh -c \
  "command -v git >/dev/null && command -v ssh >/dev/null && test \"\$(cat /opt/spawnr-environment-sentinel)\" = $environment_sentinel"
"$control" --socket "$fresh_socket" /bin/sh -c \
  "test ! -e /workspace/private-sentinel"
"$control" --socket "$fresh_socket" /bin/sh -c 'test ! -e /run/spawnr/gh-token'

echo 'Spawnr publish isolation E2E passed.'
