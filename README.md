# Spawnr

Spawnr turns an OCI development environment and a Git repository into an
isolated, persistent KVM development computer.

```text
Environment × Source → Development Computer
```

The repository is cloned inside the guest. It never needs to be checked out on
the host.

```console
$ spawnr clone \
    ghcr.io/acme/rust-dev:v1 \
    git@github.com:acme/project.git

✓ created project-1
✓ cloned git@github.com:acme/project.git

Ready:
  spawnr open project-1

$ spawnr open project-1
```

After changing the machine's toolchain or system configuration, promote only
that environment to another standard OCI image:

```console
$ spawnr publish project-1 ghcr.io/acme/rust-dev:v2
✓ published environment from project-1 to ghcr.io/acme/rust-dev:v2
  workspace and session storage were excluded structurally
```

## The three-domain model

Every machine has three separate storage and security domains:

- **Environment** is a persistent raw ext4 root disk. Changes under paths such
  as `/usr`, `/usr/local`, `/opt`, and `/etc` survive stop/start and are the
  only machine state accepted by `spawnr publish`.
- **Workspace** is a different persistent ext4 disk mounted at `/workspace`.
  It holds the repository, `.git`, build output, and project-local state. The
  publishing code is never given this disk.
- **Session** is ephemeral state. The guest mounts `/run/spawnr` as tmpfs for
  sanitized Git identity, a GitHub token, known-host records, and the Unix end
  of the forwarded SSH-agent connection. `open` also sets Bash `HISTFILE`
  there. The host-side runtime directory is cleared between boots.

This is a structural boundary, not a best-effort cleanup pass. See
[Architecture](docs/architecture.md) for the full design.

```text
                    HOST
                      │
                   Spawnr
                      │
            Cloud Hypervisor / KVM
                      │
         ┌────────────────────────────┐
         │ Development VM             │
 OCI ←──│ ENVIRONMENT  persistent    │
         │ WORKSPACE    persistent    │ → never publish
         │ SESSION      ephemeral     │ → never publish
         └────────────────────────────┘
```

## Runtime, not Docker

Spawnr is Linux-first and boots each development computer with Cloud
Hypervisor on KVM. It pulls and unpacks registry images directly with `skopeo`
and `umoci`, then materializes the OCI root filesystem as ext4. Publishing uses
`umoci repack --no-mask-volumes` to produce correct OCI changes and whiteouts
for the complete environment filesystem. Registry pulls are pinned to their
resolved digest before download.

The production path does **not** use Docker Engine, the Docker CLI,
`/var/run/docker.sock`, or another container daemon. In `skopeo` terminology,
`docker://` means the registry transport; it does not mean a Docker daemon.
Spawnr explicitly rejects the `docker-daemon:` transport.

## Requirements

The current V1 target is Linux x86_64. Starting a machine requires:

- accessible `/dev/kvm` with KVM API version 12;
- Cloud Hypervisor and `passt`;
- an uncompressed guest `vmlinux` plus the Spawnr initramfs;
- `mkfs.ext4`;
- a host resolver with an IPv4 nameserver.

Direct OCI creation additionally uses `skopeo`, `umoci`, `unshare`, `du`, a
static `spawnr-agent`, and static BusyBox. Publishing additionally requires
`e2fsck`, `fuse2fs`, and `fusermount3` or `fusermount`. Rootless OCI ownership
mapping requires working user namespaces and subordinate UID/GID mappings.

The portable release CLI installs its matched runtime itself, then verifies
the host and runtime separately:

```console
$ spawnr setup
$ spawnr doctor
```

`setup` downloads the exact HTTPS archive locked into that CLI, verifies its
size and SHA-256, validates every archive member and manifest entry, and
activates it atomically below `$XDG_DATA_HOME/spawnr/runtime/`. It does not use
Docker, curl, or distribution runtime packages. Nix bundles provide the same
components directly and therefore do not require this setup step.

The detailed source build, guest asset, local install, and end-to-end
validation procedure is in [Development](docs/development.md).
The public distribution trust chain is specified in [Release and runtime
contract](docs/releases.md).

### Reproducible Nix build

On Linux x86_64 with flakes enabled, the pinned source build is:

```console
$ nix flake check
$ nix build
$ ./result/bin/spawnr doctor
```

`nix build` produces a runnable bundle containing the host CLI, static guest
agent and BusyBox, a matched guest kernel/initramfs, Cloud Hypervisor, and the
OCI/network/filesystem tools. It does not introduce a Docker daemon. Enter the
same pinned Rust 1.88 development environment with `nix develop`; Nix remains
optional for users of future native packages.

Release maintainers can build the portable musl CLI and complete candidate
artifact directory with `nix build .#spawnr-static` and
`nix build .#release-artifacts`. The latter includes the deterministic runtime
archive and candidate lock embedded in the static CLI, checksums, notices,
source inventory, and SPDX SBOM.

The release workflow rebuilds those artifacts on two independent GitHub
runners, requires byte equality, exercises the exact files on a protected KVM
runner, and only then permits an attested immutable GitHub Release. See the
[release contract](docs/releases.md#github-actions-release-gates) for the
repository settings and two-tag publication sequence.

## Basic use

Create a machine without a repository:

```console
$ spawnr init scratch --environment docker.io/library/ubuntu:24.04
$ spawnr start scratch
$ spawnr open scratch
```

Create independent machines from one environment and repository:

```console
$ spawnr clone ghcr.io/acme/rust-dev:v1 https://github.com/acme/project.git --count 3
$ spawnr ls
```

Stop and restart without losing environment or workspace changes:

```console
$ spawnr stop project-1
$ spawnr start project-1
```

Remove a machine. Spawnr refuses to remove a Git workspace with obvious
uncommitted changes unless the destructive intent is explicit:

```console
$ spawnr rm project-1
$ spawnr rm project-1 --force
```

See [CLI reference](docs/cli.md) for names, output formats, data-directory
selection, local OCI layouts, and exact command semantics.

## Environment image contract

Spawnr treats an OCI image as a Linux filesystem distribution, not as a
container application. Its entrypoint and command are not run. Spawnr injects
its static guest agent and BusyBox, then boots the agent as PID 1.

An environment used by `init` currently needs `/bin/sh`, `/bin/bash`, and
`useradd` so the `dev` account can be created. An environment used by `clone`
also needs Git; SSH repository URLs need an SSH client. A source image with a
real `sudo` binary gets a passwordless policy for `dev`; otherwise Spawnr
installs a small passwordless `sudo COMMAND` fallback inside this single-user
VM.

## Host identity

The guest owns tools and source; the host owns user identity. On each start or
open, Spawnr exposes only selected values:

- `user.name`, `user.email`, and SSH signing configuration from global Git
  configuration;
- `GH_TOKEN` or `GITHUB_TOKEN`, when set;
- public system/user SSH known-host records;
- the capability to ask the host's `SSH_AUTH_SOCK` to sign.

Private SSH keys and the host Git configuration are not copied. Session values
live under guest tmpfs and are not inputs to publishing. Agent forwarding is a
powerful capability: code in a running guest can request signatures from the
forwarded agent. Read [Security model](docs/security.md) before using Spawnr
with untrusted code.

## Current scope

- Linux x86_64 and KVM only.
- Linux/amd64 OCI environments only.
- Four vCPUs and 4 GiB RAM per machine are currently fixed defaults.
- Networking is outbound-only; incoming port forwarding is not implemented.
- `spawnr open` uses a private vsock PTY. An SSH server, generated SSH config,
  and IDE Remote SSH integration are future work.
- OCI signature policy verification is not yet enabled. Registry TLS and
  content digests still apply; see the security documentation for the exact
  distinction.

## Validation status

The full local OCI workflow has been exercised on the development host with
Cloud Hypervisor 53 and KVM API 12: daemonless Ubuntu pull and ext4
materialization, guest boot, vsock health/exec, `dev` and passwordless sudo,
passt DHCP/DNS/HTTP, environment and workspace persistence across stop/start,
in-guest Git clone, SSH-agent forwarding, a real interactive PTY, local OCI
publish, and reimport into a fresh VM. The reimported environment contained its
installed Git/OpenSSH tools and environment sentinel; the workspace and session
sentinels were absent. The committed critical regression runs the contiguous
`clone → open → mutate → restart → publish → fresh import` workflow and scans
the unmounted OCI rootfs for workspace/session secrets. Reproduction commands
are in [Development](docs/development.md).

## Documentation

- [Architecture](docs/architecture.md)
- [CLI reference](docs/cli.md)
- [Security model](docs/security.md)
- [Development and validation](docs/development.md)

Spawnr is licensed under [Apache-2.0](LICENSE).
