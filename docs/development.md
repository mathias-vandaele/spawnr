# Development and validation

This repository is a Rust workspace with host code, a statically linked guest
agent, an initramfs builder, and a small test-only protocol client. No Docker
daemon or Docker CLI is needed to build, test, run, or publish Spawnr images.

## Repository layout

```text
crates/spawnr/           host CLI and lifecycle implementation
crates/spawnr-agent/     guest PID 1 and control plane
crates/spawnr-protocol/  shared framed protocol
guest/assets/            initramfs init and guest DHCP/systemd assets
scripts/build-guest-assets.sh
scripts/e2e-control.py   development-only non-TTY vsock client
scripts/e2e-critical.sh  clone/open/restart/publish/reimport KVM test
scripts/e2e-lifecycle.sh clone/open/persistence/dirty-rm KVM test
scripts/e2e-publish.sh   offline-verified OCI publish isolation test
scripts/test-oci-volume.sh focused OCI declared-volume layer test
docs/
```

## Runtime dependencies

The minimum supported Rust version is 1.88; `rust-toolchain.toml` pins the
development toolchain, components, and musl target to 1.88.0. Runtime components are resolved
from an environment override, then `$SPAWNR_HOME/bin`, then `PATH` where
appropriate.

| Purpose | Required components |
|---|---|
| Build host | Rust/Cargo 1.88+, C toolchain, `pkg-config` |
| Build static guest | musl Rust target and musl C linker |
| Build boot assets | uncompressed x86_64 `vmlinux`, matching kernel module directory, static BusyBox, `modprobe`, `cpio`, `gzip` |
| Boot VM | `/dev/kvm`, Cloud Hypervisor, `passt`, `mkfs.ext4`, guest kernel/initramfs |
| Pull/materialize OCI | `skopeo`, `umoci`, `unshare`, `mkfs.ext4`, GNU `du`, static guest agent, static BusyBox |
| Publish OCI | all OCI tools plus `e2fsck`, `fuse2fs`, and `fusermount3` or `fusermount` |

OCI ownership is unpacked in a user namespace using `unshare --map-auto
--map-root-user`. Configure subordinate ranges for the invoking account in
`/etc/subuid` and `/etc/subgid`, and install the distribution's `newuidmap` and
`newgidmap` helpers when its user-namespace setup requires them. Publishing
also requires usable `/dev/fuse` access.

The VMM command line has been validated with Cloud Hypervisor 53. A different
release must support the same direct-kernel, raw-disk, vhost-user net,
hybrid-vsock, API socket, and Landlock options.

The recorded end-to-end validation used Cloud Hypervisor 53.0, passt
2026_07_16, skopeo 1.24.0, umoci 0.6.0, BusyBox 1.37.0, and Linux 6.18.42.
These versions are a reproducibility baseline, not a claim that compatible
newer releases are unsupported. Cargo dependencies are fixed by `Cargo.lock`;
the kernel and initramfs must always be built from the same pinned kernel
source/configuration and module output.

Tool/asset override variables are:

```text
SPAWNR_CLOUD_HYPERVISOR  SPAWNR_PASST
SPAWNR_KERNEL            SPAWNR_INITRAMFS
SPAWNR_SKOPEO            SPAWNR_UMOCI
SPAWNR_UNSHARE           SPAWNR_MKFS_EXT4
SPAWNR_E2FSCK            SPAWNR_FUSE2FS
SPAWNR_FUSERMOUNT        SPAWNR_DU
SPAWNR_AGENT             SPAWNR_BUSYBOX
```

## Reproducible Nix flake

The flake pins nixpkgs, the Rust overlay, Rust 1.88.0, every Cargo dependency,
the guest kernel/module pair, BusyBox, Cloud Hypervisor, passt, and OCI tools.
It supports the current product platform, `x86_64-linux`.

Run the complete source gate:

```console
$ nix flake check
```

This builds the host CLI, the statically linked musl guest agent, guest boot
assets, and the final runtime bundle. It also runs formatting, all workspace
tests, Clippy with warnings denied, and a static-link check for the guest
agent. KVM end-to-end tests remain explicit because `nix flake check` builds
inside a sandbox without `/dev/kvm`.

Build or run the complete pinned bundle:

```console
$ nix build
$ ./result/bin/spawnr doctor
$ nix run . -- doctor
```

Build individual outputs when iterating:

```console
$ nix build .#spawnr
$ nix build .#spawnr-static
$ nix build .#spawnr-agent
$ nix build .#guest-assets
$ nix build .#runtime-tree
$ nix build .#runtime-archive
$ nix build .#runtime-lock-candidate
$ nix build .#release-artifacts
```

The static CLI and managed runtime can be checked outside the Nix store with:

```console
$ nix build .#spawnr-static -o result-cli
$ nix build .#runtime-archive -o result-runtime
$ scripts/check-release-relocation.sh \
    result-cli/bin/spawnr \
    result-runtime/spawnr-runtime-0.1.0-x86_64-linux.tar.zst
```

This isolates the extracted files with Bubblewrap, verifies every manifest
digest, rejects dynamic ELF interpreters, exercises ext4 creation/checking,
and proves that the bundled skopeo has no Docker-daemon transport.

Exercise the exact offline setup path used by release validation with:

```console
$ nix build .#spawnr-static -o result-cli
$ nix build .#runtime-archive -o result-runtime
$ nix build .#runtime-lock-candidate -o result-lock
$ export SPAWNR_HOME=/tmp/spawnr-setup-test
$ result-cli/bin/spawnr setup \
    --runtime-lock result-lock/runtime.lock.json \
    --runtime-archive result-runtime/spawnr-runtime-0.1.0-x86_64-linux.tar.zst
$ result-cli/bin/spawnr doctor
```

Enter the development environment:

```console
$ nix develop
$ cargo test --workspace --locked
$ cargo build --release --locked -p spawnr
$ cargo build --release --locked \
    --target x86_64-unknown-linux-musl \
    -p spawnr-agent
```

The default bundle uses a wrapper to pass immutable Nix store paths through
Spawnr's existing `SPAWNR_*` tool/asset overrides. Mutable machine state still
goes to `SPAWNR_HOME` (or the normal XDG data directory); no Nix store path is
hard-coded into the Rust source.

The public CLI/runtime versioning, manifest, digest chain, and release
preparation transaction are specified in [Release and runtime
contract](releases.md).

GitHub workflow changes are checked by `actionlint`; release shell helpers are
checked by ShellCheck and the public release preflight has behavioral Python
tests as part of `nix flake check`. Release workflow manual runs require a
GitHub-hosted `ubuntu-24.04` runner that exposes KVM. The release preflight
proves KVM, FUSE, and subordinate user namespaces before expensive builds;
normal pull-request CI does not boot a VM.

## Build the Rust workspace

With rustup and a system `musl-gcc`:

```console
$ rustup toolchain install 1.88.0
$ rustup target add --toolchain 1.88.0 x86_64-unknown-linux-musl
$ cargo +1.88.0 build --locked --release -p spawnr
$ CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
    cargo +1.88.0 build --locked --release \
    --target x86_64-unknown-linux-musl \
    -p spawnr-agent
$ file target/x86_64-unknown-linux-musl/release/spawnr-agent
```

The last command must report a static or static-PIE executable. A dynamically
linked agent is not suitable for injection into arbitrary OCI filesystems.

Run source-level checks:

```console
$ cargo +1.88.0 fmt --all --check
$ cargo +1.88.0 test --workspace --locked
$ cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
```

Unit tests cover name/reference parsing, atomic state, storage markers and raw
ext4 images, sparse/reflink cloning, PID identity, VMM command construction,
protocol framing, mount/path validation, and session-value validation. They do
not replace a real KVM/OCI end-to-end run.

The focused OCI fixture proves that changes beneath an image-declared
`Config.Volumes` path survive the exact `umoci --no-mask-volumes` repack
semantics used by Spawnr:

```console
$ ./scripts/test-oci-volume.sh
```

## Build the guest kernel and initramfs assets

Choose an uncompressed Linux x86_64 kernel, its exact
`lib/modules/<kernel-version>` directory, and static BusyBox. The kernel/module
configuration must support KVM guests plus virtio PCI, block, net, console,
vsock, ext4, devtmpfs, devpts, tmpfs, and AF_PACKET.

```console
$ export SPAWNR_GUEST_KERNEL=/path/to/vmlinux
$ export SPAWNR_GUEST_MODULES=/path/to/lib/modules/6.x.y
$ export SPAWNR_BUSYBOX=/path/to/static/busybox
$ ./scripts/build-guest-assets.sh ./guest/build
```

The script computes and copies the required module dependency closure, then
creates:

```text
guest/build/vmlinux
guest/build/initramfs
```

The kernel and module directory must be from the same build. The initramfs
loads the devices Spawnr needs, mounts `/dev/vda`, and switches directly to
the injected agent.

## Install a local development build

Use a short absolute data path: AF_UNIX endpoint names have a 108-byte limit,
and each machine path includes a UUID.

```console
$ export SPAWNR_HOME=/tmp/spawnr-dev
$ install -d -m 0700 "$SPAWNR_HOME/bin"
$ install -m 0755 target/release/spawnr "$SPAWNR_HOME/bin/spawnr"
$ install -m 0755 \
    target/x86_64-unknown-linux-musl/release/spawnr-agent \
    "$SPAWNR_HOME/bin/spawnr-agent"
$ install -m 0755 "$SPAWNR_BUSYBOX" "$SPAWNR_HOME/bin/spawnr-busybox"
$ install -m 0644 guest/build/vmlinux "$SPAWNR_HOME/bin/vmlinux"
$ install -m 0644 guest/build/initramfs "$SPAWNR_HOME/bin/initramfs"
```

Install or symlink Cloud Hypervisor, `passt`, `skopeo`, and `umoci` into that
`bin` directory, leave them on `PATH`, or set their override variables.
Filesystem/user-namespace tools may likewise come from `PATH`.

```console
$ "$SPAWNR_HOME/bin/spawnr" doctor
```

`doctor` is read-only and checks boot prerequisites. OCI and publish-only
tools are checked when their respective operations execute.

## Verified real-VM smoke test

The following procedure has passed on the development host with KVM API 12 and
Cloud Hypervisor 53. It exercises direct registry pull, ext4 creation, KVM boot,
Cloud Hypervisor hybrid vsock, guest account setup, passt DHCP/DNS, and clean
stop/start. It uses no Docker fixture.

```console
$ export PATH="$SPAWNR_HOME/bin:$PATH"
$ spawnr init smoke --environment docker.io/library/ubuntu:24.04
$ spawnr start smoke
$ machine_id=$(jq -r '.machines.smoke.id' "$SPAWNR_HOME/state.json")
$ control="$SPAWNR_HOME/machines/$machine_id/session/vsock.sock"
$ ./scripts/e2e-control.py --socket "$control" /usr/bin/id
$ ./scripts/e2e-control.py --socket "$control" sudo /usr/bin/id
$ ./scripts/e2e-control.py --socket "$control" /usr/libexec/spawnr-busybox wget -qO- http://example.com/
$ spawnr stop smoke
$ spawnr start smoke
$ spawnr stop smoke
$ spawnr rm smoke --force
```

Expected identity is `dev` for the first `id` and real/effective UID 0 for the
second. The HTTP request checks guest DHCP, DNS, and outbound networking.

## Verified end-to-end tests

The critical test is the specification's complete user workflow in one run:
signature `clone`, a real `open` shell, environment/workspace/session checks,
stop/start persistence, publish from running and stopped states, full offline
artifact leak inspection, a simulated VMM crash, destruction of the original,
fresh-VM reimport, failed-clone rollback, and unreachable-registry cleanup.
It requires a fresh, empty, disposable `SPAWNR_HOME` because its final cleanup
assertion rejects unrelated machines. Point it at an environment which already
contains Bash, Git, SSH, and CA roots:

```console
$ export SPAWNR_HOME=/tmp/spawnr-critical-e2e
$ export PATH="$SPAWNR_HOME/bin:$PATH"
$ export SPAWNR_E2E_ENVIRONMENT=oci:/tmp/spawnr-dev-environment:v1
$ ./scripts/e2e-critical.sh
```

The committed lifecycle test exercises the signature `clone` command, a real
`open` PTY (including a descendant which retains the PTY), environment and
workspace persistence, idempotent start/stop, dirty-workspace refusal, and
forced cleanup. Point it at an environment which already contains Bash, Git,
SSH, and CA roots:

```console
$ export SPAWNR_HOME=/tmp/spawnr-lifecycle-e2e
$ export PATH="$SPAWNR_HOME/bin:$PATH"
$ export SPAWNR_E2E_ENVIRONMENT=oci:/tmp/spawnr-published:v2
$ ./scripts/e2e-lifecycle.sh
```

The publish test begins with Ubuntu, installs Git/OpenSSH in the environment,
places distinct sentinels in environment, workspace, and session domains,
checks stop/start persistence, publishes to a local OCI layout, and boots a
fresh VM from it:

```console
$ export SPAWNR_HOME=/tmp/spawnr-publish-e2e
$ export PATH="$SPAWNR_HOME/bin:$PATH"
$ ./scripts/e2e-publish.sh
```

Before the fresh VM boots, the script unpacks the published artifact offline
in the same mapped user namespace. It searches the entire OCI rootfs,
including the underlying `/workspace` and `/run` paths that guest mounts
would otherwise hide, for both workspace and session sentinels. It then checks
that the fresh guest has the installed tools and environment sentinel but no
old workspace or session state.

All three scripts use direct `skopeo`/`umoci` OCI paths and no Docker fixture.
The helper `scripts/e2e-control.py` is development-only; it speaks the same
private framed protocol as the Rust client to make shell assertions
deterministic.

## OCI test fixtures

Prefer registry references and local OCI layouts in tests. If a developer uses
Docker solely to obtain a disposable rootfs while experimenting, keep that
fixture outside the production code and assertions. No accepted runtime or
publish test may require Docker Engine, Docker CLI, or `docker.sock`.

## Debugging failures

- Run with `-v` to expose OCI/VMM subprocess diagnostics.
- Inspect `state.json` only while no Spawnr command is mutating it; never edit
  it by hand.
- VMM, passt, and serial logs are under
  `$SPAWNR_HOME/machines/<uuid>/session/` while that session exists.
- A socket-path error means the data root is too long; retry with a shorter
  absolute `SPAWNR_HOME`.
- A mapped-OCI error usually indicates missing subordinate UID/GID ranges or
  disabled unprivileged user namespaces.
- A publish mount error usually indicates missing `fuse2fs`, `fusermount`, or
  `/dev/fuse` access.
- `clone` failures that report missing Git or SSH must be fixed in the OCI
  environment, not by installing project tools on the host.

When changing storage or publishing code, rerun the full publish isolation
test. Unit tests alone cannot prove that a workspace sentinel is absent from a
real OCI artifact.
