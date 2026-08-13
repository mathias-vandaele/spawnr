#!/usr/bin/env bash
set -euo pipefail

: "${SPAWNR_HOME:?set SPAWNR_HOME to a disposable, fully installed Spawnr data root}"
: "${SPAWNR_E2E_ENVIRONMENT:?set SPAWNR_E2E_ENVIRONMENT to an OCI environment containing bash, Git, SSH, and CA roots}"

spawnr_bin=${SPAWNR_BIN:-spawnr}
control=${SPAWNR_E2E_CONTROL:-"$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/e2e-control.py"}
process_control=${SPAWNR_E2E_PROCESS:-"$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/e2e-process.py"}
repository=${SPAWNR_E2E_REPOSITORY:-https://github.com/octocat/Hello-World.git}
umoci_bin=${SPAWNR_UMOCI:-umoci}
unshare_bin=${SPAWNR_UNSHARE:-unshare}
original=spawnr-e2e-critical
fresh=spawnr-e2e-critical-fresh
failed_clone=spawnr-e2e-critical-failed-clone
unreachable=spawnr-e2e-critical-unreachable
environment_sentinel=SPAWNR_CRITICAL_ENVIRONMENT_BC6D43F1
workspace_sentinel=SPAWNR_CRITICAL_WORKSPACE_1E78A2C9
session_sentinel=SPAWNR_CRITICAL_SESSION_8A0F51D4
history_sentinel=SPAWNR_CRITICAL_HISTORY_732E09AB
layout=$(mktemp -d /tmp/spawnr-critical-published.XXXXXX)
offline_bundle="${layout}.offline-bundle"
open_log=$(mktemp /tmp/spawnr-critical-open.XXXXXX)
failure_probe=$(mktemp -d /tmp/spawnr-critical-failure.XXXXXX)
failure_log="$failure_probe/clone.log"

machine_field() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["machines"][sys.argv[2]][sys.argv[3]])' \
    "$SPAWNR_HOME/state.json" "$1" "$2"
}

machine_socket() {
  printf '%s/machines/%s/session/vsock.sock\n' \
    "$SPAWNR_HOME" "$(machine_field "$1" id)"
}

cleanup() {
  "$spawnr_bin" rm "$original" --force >/dev/null 2>&1 || true
  "$spawnr_bin" rm "$fresh" --force >/dev/null 2>&1 || true
  "$spawnr_bin" rm "$failed_clone" --force >/dev/null 2>&1 || true
  "$spawnr_bin" rm "$unreachable" --force >/dev/null 2>&1 || true
  if [[ -e "$offline_bundle" ]]; then
    "$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
      /bin/rm -rf -- "$offline_bundle" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$layout"
  rm -rf -- "$failure_probe"
  rm -f -- "$open_log"
}
trap cleanup EXIT

export GH_TOKEN=$session_sentinel
"$spawnr_bin" clone "$SPAWNR_E2E_ENVIRONMENT" "$repository" --name "$original"

# A duplicate explicit name must fail without modifying the existing machine.
if "$spawnr_bin" clone "$SPAWNR_E2E_ENVIRONMENT" "$repository" --name "$original"; then
  echo 'duplicate machine name unexpectedly succeeded' >&2
  exit 1
fi

# Exercise the public interactive path, prove that the session token is
# available, and verify that Spawnr's HISTFILE setting puts this fixture's Bash
# history in session tmpfs. Environment/workspace sentinels are introduced by
# direct control requests so their provenance remains unambiguous.
printf '%s\n' \
  'printf "SPAWNR_CRITICAL_OPEN:%s:%s\n" "$USER" "$PWD"' \
  "printf '%s\\n' $history_sentinel" \
  'test -n "${GH_TOKEN:-}"' \
  'exit' |
  timeout 30 script -qec "$spawnr_bin open $original" "$open_log"
grep -F "SPAWNR_CRITICAL_OPEN:dev:/workspace/$(machine_field "$original" repository_dir)" "$open_log"

socket=$(machine_socket "$original")
repository_dir=$(machine_field "$original" repository_dir)
"$control" --socket "$socket" /bin/grep -F "$history_sentinel" \
  /run/spawnr/bash-history
"$control" --socket "$socket" sudo /bin/sh -c \
  "printf %s $environment_sentinel >/opt/spawnr-critical-environment"
"$control" --socket "$socket" --cwd "/workspace/$repository_dir" /bin/sh -c \
  "printf %s $workspace_sentinel >spawnr-critical-workspace"

"$spawnr_bin" stop "$original"
"$spawnr_bin" start "$original"
socket=$(machine_socket "$original")
"$control" --socket "$socket" --cwd "/workspace/$repository_dir" /bin/sh -c \
  "test \"\$(cat /opt/spawnr-critical-environment)\" = $environment_sentinel && test \"\$(cat spawnr-critical-workspace)\" = $workspace_sentinel"

# Simulate a VMM crash through a pidfd after revalidating the complete Spawnr
# process identity. Passt must exit with its sole client, and start must
# reconcile both stale helpers.
machine_id=$(machine_field "$original" id)
"$process_control" signal \
  "$SPAWNR_HOME/machines/$machine_id/session/cloud-hypervisor.pid.json" KILL
sleep 1
"$spawnr_bin" start "$original"
socket=$(machine_socket "$original")
"$control" --socket "$socket" --cwd "/workspace/$repository_dir" /bin/sh -c \
  "test \"\$(cat /opt/spawnr-critical-environment)\" = $environment_sentinel && test \"\$(cat spawnr-critical-workspace)\" = $workspace_sentinel"

# Publishing a running VM must stop it consistently and restore its prior
# state. Only the environment disk is passed into this operation.
"$spawnr_bin" publish "$original" "oci:$layout:v2"
"$spawnr_bin" start "$original" | grep -F 'already running'

# Inspect the complete artifact offline, before fresh guest mounts can hide
# the image's underlying /workspace or /run paths.
"$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
  "$umoci_bin" unpack --image "$layout:v2" "$offline_bundle"
"$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
  /bin/sh -eu -c '
    root=$1
    environment=$2
    workspace=$3
    session=$4
    history=$5
    test "$(cat "$root/opt/spawnr-critical-environment")" = "$environment"
    for secret in "$workspace" "$session" "$history"; do
      if find "$root" -xdev -type f -size -8M -exec grep -lF "$secret" {} + 2>/dev/null | grep -q .; then
        echo "secret leaked into published environment: $secret" >&2
        exit 1
      fi
    done
    test ! -e "$root/run/spawnr/gh-token"
  ' spawnr-critical-offline-check "$offline_bundle/rootfs" \
  "$environment_sentinel" "$workspace_sentinel" "$session_sentinel" "$history_sentinel"

# The stopped-machine path must publish without unexpectedly starting it.
"$spawnr_bin" stop "$original"
"$spawnr_bin" publish "$original" "oci:$layout:stopped"
"$spawnr_bin" stop "$original" | grep -F 'already stopped'
"$spawnr_bin" rm "$original" --force

unset GH_TOKEN GITHUB_TOKEN SSH_AUTH_SOCK
"$spawnr_bin" init "$fresh" --environment "oci:$layout:v2"
"$spawnr_bin" start "$fresh"
fresh_socket=$(machine_socket "$fresh")
"$control" --socket "$fresh_socket" /bin/sh -c \
  "command -v git >/dev/null && test \"\$(cat /opt/spawnr-critical-environment)\" = $environment_sentinel"
"$control" --socket "$fresh_socket" /bin/sh -c \
  'test ! -e /workspace/spawnr-critical-workspace && test ! -e /run/spawnr/gh-token && test ! -e /run/spawnr/bash-history'
"$spawnr_bin" rm "$fresh" --force

# A guest-side Git failure must roll back the newly created machine and every
# owned helper/disk. Run it asynchronously so the test can retain the exact
# helper identities and machine UUID before rollback removes their files.
"$spawnr_bin" clone "$SPAWNR_E2E_ENVIRONMENT" \
  https://127.0.0.1:9/spawnr/does-not-exist.git --name "$failed_clone" \
  >"$failure_log" 2>&1 &
failed_clone_command=$!
failed_machine_id=
for _ in $(seq 1 1000); do
  failed_machine_id=$(python3 -c '
import json, pathlib, sys
try:
    state = json.loads(pathlib.Path(sys.argv[1]).read_text())
    print(state["machines"].get(sys.argv[2], {}).get("id", ""))
except (FileNotFoundError, json.JSONDecodeError):
    pass
' "$SPAWNR_HOME/state.json" "$failed_clone")
  if [[ -n "$failed_machine_id" ]]; then
    session="$SPAWNR_HOME/machines/$failed_machine_id/session"
    [[ -f "$failure_probe/cloud-hypervisor.pid.json" || ! -f "$session/cloud-hypervisor.pid.json" ]] || \
      cp -- "$session/cloud-hypervisor.pid.json" "$failure_probe/cloud-hypervisor.pid.json"
    [[ -f "$failure_probe/passt.pid.json" || ! -f "$session/passt.pid.json" ]] || \
      cp -- "$session/passt.pid.json" "$failure_probe/passt.pid.json"
    if [[ -f "$failure_probe/cloud-hypervisor.pid.json" && -f "$failure_probe/passt.pid.json" ]]; then
      break
    fi
  fi
  kill -0 "$failed_clone_command" 2>/dev/null || break
  sleep 0.01
done
if wait "$failed_clone_command"; then
  echo 'unreachable Git repository unexpectedly cloned' >&2
  exit 1
fi
cat "$failure_log" >&2
test -n "$failed_machine_id"
test -f "$failure_probe/cloud-hypervisor.pid.json"
test -f "$failure_probe/passt.pid.json"
"$process_control" wait-dead "$failure_probe/cloud-hypervisor.pid.json"
"$process_control" wait-dead "$failure_probe/passt.pid.json"
test ! -e "$SPAWNR_HOME/machines/$failed_machine_id"

# A registry failure happens before machine ownership is committed and must
# likewise leave no state or storage behind.
if "$spawnr_bin" init "$unreachable" \
  --environment 127.0.0.1:9/spawnr/unreachable:latest; then
  echo 'unreachable OCI registry unexpectedly succeeded' >&2
  exit 1
fi
"$spawnr_bin" ls --json | python3 -c 'import json,sys; assert json.load(sys.stdin) == []'
test -z "$(find "$SPAWNR_HOME/machines" -mindepth 1 -maxdepth 1 -print -quit)"

echo 'Spawnr critical clone/open/restart/publish/reimport E2E passed.'
