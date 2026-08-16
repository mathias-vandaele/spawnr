#!/usr/bin/env bash
set -euo pipefail

: "${SPAWNR_HOME:?set SPAWNR_HOME to a disposable, fully installed Spawnr data root}"
: "${SPAWNR_E2E_ENVIRONMENT:?set SPAWNR_E2E_ENVIRONMENT to an OCI environment containing bash, Git, SSH, and CA roots}"

spawnr_bin=${SPAWNR_BIN:-spawnr}
control=${SPAWNR_E2E_CONTROL:-"$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/e2e-control.py"}
repository=${SPAWNR_E2E_REPOSITORY:-https://github.com/octocat/Hello-World.git}
name=spawnr-e2e-lifecycle
environment_sentinel=SPAWNR_LIFECYCLE_ENVIRONMENT_50C831A4
workspace_sentinel=SPAWNR_LIFECYCLE_WORKSPACE_9D81B65E
open_log=$(mktemp /tmp/spawnr-open.XXXXXX)
error_log=$(mktemp /tmp/spawnr-rm.XXXXXX)

machine_socket() {
  local machine_id
  machine_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["machines"][sys.argv[2]]["id"])' \
    "$SPAWNR_HOME/state.json" "$name")
  printf '%s/machines/%s/session/vsock.sock\n' "$SPAWNR_HOME" "$machine_id"
}

cleanup() {
  "$spawnr_bin" rm "$name" --force >/dev/null 2>&1 || true
  rm -f -- "$open_log" "$error_log"
}
trap cleanup EXIT

"$spawnr_bin" clone "$SPAWNR_E2E_ENVIRONMENT" "$repository" --name "$name"
"$spawnr_bin" start "$name" | grep -F 'already running'
socket=$(machine_socket)
"$control" --socket "$socket" --cwd /workspace/hello-world /bin/sh -c \
  'test "$(id -un)" = dev && test "$(git rev-parse --is-inside-work-tree)" = true'

# Exercise the user-facing PTY. The disowned process deliberately retains a
# PTY descriptor; open must still return as soon as the interactive shell exits.
printf '%s\n' \
  'printf "SPAWNR_OPEN_OK:%s:%s\n" "$USER" "$PWD"' \
  'sleep 30 & disown' \
  'exit' |
  timeout 20 script -qec "$spawnr_bin open $name" "$open_log"
grep -F "SPAWNR_OPEN_OK:dev:/workspace/hello-world" "$open_log"

socket=$(machine_socket)
"$control" --socket "$socket" sudo /bin/sh -c \
  "printf %s $environment_sentinel >/opt/spawnr-lifecycle-sentinel"
"$control" --socket "$socket" /bin/sh -c \
  "printf %s $workspace_sentinel >/workspace/hello-world/spawnr-dirty"

"$spawnr_bin" stop "$name"
"$spawnr_bin" stop "$name" | grep -F 'already stopped'
"$spawnr_bin" start "$name"
"$spawnr_bin" start "$name" | grep -F 'already running'
socket=$(machine_socket)
"$control" --socket "$socket" /bin/sh -c \
  "test \"\$(cat /opt/spawnr-lifecycle-sentinel)\" = $environment_sentinel && test \"\$(cat /workspace/hello-world/spawnr-dirty)\" = $workspace_sentinel"

if "$spawnr_bin" rm "$name" >"$error_log" 2>&1; then
  echo 'rm unexpectedly accepted a dirty workspace' >&2
  exit 1
fi
grep -F 'Workspace contains uncommitted changes' "$error_log"
"$spawnr_bin" rm "$name" --force
"$spawnr_bin" ls --json | python3 -c \
  'import json,sys; assert all(row["name"] != sys.argv[1] for row in json.load(sys.stdin))' "$name"

echo 'Spawnr clone/open/persistence/lifecycle E2E passed.'
