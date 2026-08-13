# Spawnr guest integration

Spawnr injects its statically linked `spawnr-agent` as
`/usr/libexec/spawnr-agent`, a static BusyBox as
`/usr/libexec/spawnr-busybox`, and the bounded DHCP hook beside them. Direct
kernel boot selects the agent as PID 1, so the source OCI image does not need
an init system. The provided systemd unit remains useful for custom images.

An OCI environment used by `init` must contain `/bin/sh`, `/bin/bash`, and
`useradd`. An environment used by `clone` must also contain Git; SSH remotes
need an SSH client and CA roots. A native `sudo` is recommended, but when it is
absent the agent installs a small passwordless command-exec fallback for this
single-user VM. Spawnr supplies block-label and networking tools through
static BusyBox. The guest kernel/initramfs contains virtio
PCI/block/net/console/vsock, AF_PACKET, and ext4 support.

Cloud Hypervisor presents the environment image as `/dev/vda`, the independent
ext4 workspace image (label `SPAWNR_WORKSPACE`) as `/dev/vdb`, and a hybrid
vsock device. At service start the agent:

1. creates the `dev` account and its narrowly named passwordless-sudo policy;
2. mounts only the filesystem bearing label `SPAWNR_WORKSPACE` at
   `/workspace`, then proves it is an exact mount with a different `st_dev`
   from `/`;
3. mounts a `nosuid,nodev` tmpfs at `/run/spawnr`; and
4. listens on vsock port 19870.

The host supplies the validated machine name on the private kernel command
line. GitHub tokens, the sanitized Git configuration, the guest SSH-agent
socket, and every other identity capability live only in `/run/spawnr`.
