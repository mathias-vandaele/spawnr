#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
    echo 'usage: test-installer.sh INSTALLER STATIC_CLI VERSION' >&2
    exit 2
fi
installer=$(realpath -- "$1")
cli=$(realpath -- "$2")
version=$3
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
test_root=$(mktemp -d /tmp/spawnr-installer-test.XXXXXX)
cleanup() {
    if [ -d "$test_root" ]; then
        chmod -R u+w "$test_root" 2>/dev/null || true
        find "$test_root" -depth -delete 2>/dev/null || true
    fi
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$test_root/fake-bin"
ln -s "$script_dir/installer-fake-curl.sh" "$test_root/fake-bin/curl"
test_path="$test_root/fake-bin:$PATH"

HOME="$test_root/home" \
SPAWNR_INSTALL_DIR="$test_root/good-bin" \
SPAWNR_INSTALL_TEST_CLI="$cli" \
PATH="$test_path" \
    "$installer"
cmp -- "$cli" "$test_root/good-bin/spawnr"
"$test_root/good-bin/spawnr" --version | grep -Fx "spawnr $version"

cp -- "$cli" "$test_root/tampered-cli"
chmod u+w "$test_root/tampered-cli"
truncate --size=-1 "$test_root/tampered-cli"
if HOME="$test_root/home" \
    SPAWNR_INSTALL_DIR="$test_root/bad-bin" \
    SPAWNR_INSTALL_TEST_CLI="$test_root/tampered-cli" \
    PATH="$test_path" \
    "$installer"; then
    echo 'installer accepted a CLI that differs from its embedded digest' >&2
    exit 1
fi
test ! -e "$test_root/bad-bin/spawnr"
