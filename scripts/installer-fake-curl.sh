#!/bin/sh
set -eu

: "${SPAWNR_INSTALL_TEST_CLI:?set the CLI copied by the installer test transport}"
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            shift
            [ "$#" -gt 0 ] || exit 2
            output=$1
            ;;
    esac
    shift
done
[ -n "$output" ] || exit 2
cp -- "$SPAWNR_INSTALL_TEST_CLI" "$output"
