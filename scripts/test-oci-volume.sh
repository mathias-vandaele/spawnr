#!/usr/bin/env bash
set -euo pipefail

umoci_bin=${SPAWNR_UMOCI:-umoci}
unshare_bin=${SPAWNR_UNSHARE:-unshare}
root=$(mktemp -d /tmp/spawnr-volume-test.XXXXXX)
layout="$root/layout"
base_bundle="$root/base-bundle"
bundle="$root/bundle"
fresh="$root/fresh"
sentinel=SPAWNR_OCI_VOLUME_SENTINEL_0E9147BC
deleted=SPAWNR_OCI_VOLUME_DELETED_79D25F10

cleanup() {
  "$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
    /bin/rm -rf -- "$root" >/dev/null 2>&1 || true
}
trap cleanup EXIT

"$unshare_bin" --user --map-auto --map-root-user --mount --fork -- \
  /bin/sh -eu -c '
    umoci=$1
    layout=$2
    base_bundle=$3
    bundle=$4
    fresh=$5
    sentinel=$6
    deleted=$7
    "$umoci" init --layout "$layout"
    "$umoci" new --image "$layout:base"
    "$umoci" config --image "$layout:base" --config.volume /opt/declared-volume
    "$umoci" unpack --image "$layout:base" "$base_bundle"
    mkdir -p "$base_bundle/rootfs/opt/declared-volume"
    printf %s "$deleted" >"$base_bundle/rootfs/opt/declared-volume/deleted"
    "$umoci" repack --no-mask-volumes --image "$layout:with-content" "$base_bundle"
    "$umoci" unpack --image "$layout:with-content" "$bundle"
    rm "$bundle/rootfs/opt/declared-volume/deleted"
    printf %s "$sentinel" >"$bundle/rootfs/opt/declared-volume/sentinel"
    "$umoci" repack --no-mask-volumes --image "$layout:published" "$bundle"
    "$umoci" unpack --image "$layout:published" "$fresh"
    test "$(cat "$fresh/rootfs/opt/declared-volume/sentinel")" = "$sentinel"
    test ! -e "$fresh/rootfs/opt/declared-volume/deleted"
  ' spawnr-volume-check "$umoci_bin" "$layout" "$base_bundle" "$bundle" "$fresh" "$sentinel" "$deleted"

echo 'OCI declared-volume addition/deletion test passed.'
