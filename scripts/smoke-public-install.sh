#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo 'usage: smoke-public-install.sh CLI_VERSION EXPECTED_INSTALLER' >&2
    exit 2
fi

version=$1
expected_installer=$2
[ -f "$expected_installer" ] || {
    echo "expected installer does not exist: $expected_installer" >&2
    exit 1
}
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
website=$(python3 -c \
    'import pathlib, sys, tomllib; print(tomllib.loads(pathlib.Path(sys.argv[1]).read_text())["website"])' \
    "$repository_root/release/config.toml")
installer_url="$website/install.sh"
smoke_root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/spawnr-public-smoke.XXXXXX")
cleanup() {
    if [ -d "$smoke_root" ]; then
        chmod -R u+w "$smoke_root" 2>/dev/null || true
        find "$smoke_root" -depth -delete 2>/dev/null || true
    fi
}
trap cleanup EXIT HUP INT TERM

ready=false
attempt=1
while [ "$attempt" -le 40 ]; do
    if curl \
        --proto '=https' \
        --tlsv1.2 \
        --fail \
        --location \
        --silent \
        --show-error \
        --connect-timeout 10 \
        --max-time 30 \
        --output "$smoke_root/install.sh" \
        "$installer_url" \
        && grep -Fx "version='$version'" "$smoke_root/install.sh" >/dev/null; then
        ready=true
        break
    fi
    printf 'Waiting for %s to serve Spawnr %s (attempt %s/40)\n' \
        "$installer_url" "$version" "$attempt"
    sleep 15
    attempt=$((attempt + 1))
done
[ "$ready" = true ] || {
    echo "public installer did not converge to Spawnr $version" >&2
    exit 1
}
cmp "$expected_installer" "$smoke_root/install.sh" || {
    echo 'public installer differs from the verified release candidate' >&2
    exit 1
}

HOME="$smoke_root/home" \
SPAWNR_INSTALL_DIR="$smoke_root/bin" \
    sh "$smoke_root/install.sh"
"$smoke_root/bin/spawnr" --version | grep -Fx "spawnr $version"

SPAWNR_HOME="$smoke_root/data" "$smoke_root/bin/spawnr" setup
SPAWNR_HOME="$smoke_root/data" "$smoke_root/bin/spawnr" setup \
    | grep -q 'already installed and verified'
SPAWNR_HOME="$smoke_root/data" "$smoke_root/bin/spawnr" doctor --json \
    > "$smoke_root/doctor.json" 2> "$smoke_root/doctor.err" || true
python3 -c '
import json, pathlib, sys
report = json.loads(pathlib.Path(sys.argv[1]).read_text())
checks = report["checks"]
runtime = [check for check in checks if check["name"] == "managed runtime"]
if len(runtime) != 1 or not runtime[0]["ok"]:
    raise SystemExit("public runtime is not healthy in spawnr doctor")
' "$smoke_root/doctor.json"
