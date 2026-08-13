#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo 'guest assets can only be built on Linux x86_64' >&2
  exit 1
fi

: "${SPAWNR_GUEST_KERNEL:?set SPAWNR_GUEST_KERNEL to an uncompressed x86_64 vmlinux}"
: "${SPAWNR_GUEST_MODULES:?set SPAWNR_GUEST_MODULES to lib/modules/<version>}"
: "${SPAWNR_BUSYBOX:?set SPAWNR_BUSYBOX to a static BusyBox binary}"

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output_dir=${1:-"$project_root/guest/build"}
stage=$(mktemp -d /tmp/spawnr-initramfs.XXXXXX)
trap 'rm -rf -- "$stage"' EXIT

install -d -m 0755 "$output_dir" "$stage/bin" "$stage/sbin" "$stage/proc" "$stage/sys" "$stage/dev" "$stage/newroot"
install -m 0755 "$SPAWNR_BUSYBOX" "$stage/bin/busybox"
for applet in sh mount modprobe switch_root mkdir; do
  ln -s busybox "$stage/bin/$applet"
done
ln -s ../bin/busybox "$stage/sbin/modprobe"
install -m 0755 "$project_root/guest/assets/init" "$stage/init"

kernel_version=$(basename -- "$SPAWNR_GUEST_MODULES")
module_dest="$stage/lib/modules/$kernel_version"
install -d -m 0755 "$module_dest"
for metadata in \
  modules.dep modules.dep.bin \
  modules.alias modules.alias.bin \
  modules.builtin modules.builtin.bin modules.builtin.alias.bin \
  modules.softdep modules.symbols modules.symbols.bin; do
  if [[ -f "$SPAWNR_GUEST_MODULES/$metadata" ]]; then
    cp -a -- "$SPAWNR_GUEST_MODULES/$metadata" "$module_dest/"
  fi
done

while IFS= read -r module; do
  module=${module#"$SPAWNR_GUEST_MODULES"/}
  source_path="$SPAWNR_GUEST_MODULES/$module"
  install -d -m 0755 "$module_dest/$(dirname -- "$module")"
  install -m 0644 "$source_path" "$module_dest/$module"
done < <(
  for name in virtio_pci virtio_blk virtio_net virtio_console vmw_vsock_virtio_transport ext4 af_packet; do
    modprobe --set-version "$kernel_version" --dirname "$(dirname -- "$(dirname -- "$(dirname -- "$SPAWNR_GUEST_MODULES")")")" --show-depends "$name"
  done | sed -n 's/^insmod //p' | sed 's/[[:space:]]*$//' | sort -u
)

cp --reflink=auto -- "$SPAWNR_GUEST_KERNEL" "$output_dir/vmlinux"
(
  cd "$stage"
  find . -print0 | cpio --null -o --format=newc --quiet | gzip -9 > "$output_dir/initramfs"
)
chmod 0644 "$output_dir/vmlinux" "$output_dir/initramfs"
echo "built $output_dir/vmlinux and $output_dir/initramfs"
