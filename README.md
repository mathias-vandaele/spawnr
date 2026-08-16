<p align="center">
  <img src="site/assets/spawnr-logo.png" width="280" alt="Spawnr logo">
</p>

<h1 align="center">Spawnr</h1>

<p align="center">
  <strong>Create isolated KVM development machines from OCI images.</strong><br>
  Keep your workspace across restarts. Keep project tools off your host.
</p>

<p align="center">
  Early preview · Linux x86_64 · Apache-2.0
</p>

## Install

```console
$ curl -fsSL https://spawnr-cli.dev/install.sh | sh
$ spawnr setup
$ spawnr doctor
```

`setup` downloads the versioned runtime that matches the CLI. \`doctor\` checks
your host and the runtime.

## Create a machine

Give Spawnr an OCI image and a Git repository:

```console
$ spawnr clone \
    ghcr.io/acme/rust-dev:v1 \
    git@github.com:acme/project.git

✓ created project-1
✓ cloned git@github.com:acme/project.git

$ spawnr open project-1
```

The repository is cloned inside the guest. You do not need a checkout on the
host.

The OCI image must contain Git. It must also contain an SSH client when you use
an SSH repository URL.

## How it works

Each machine has three separate domains:

| Domain | Contains | Lifetime |
|---|---|---|
| **Environment** | Packages, tools, and system configuration | Persists and can be published |
| **Workspace** | Source, Git data, and build output | Persists across stops and restarts |
| **Session** | Git identity, tokens, shell history, and SSH-agent access | Exists only while the machine runs |

Spawnr publishes only the environment. It excludes the workspace and session
data by design.

## Why Spawnr

- **KVM isolation:** each development machine runs with Cloud Hypervisor on KVM.
- **Daemonless OCI:** Spawnr pulls and publishes images without Docker Engine,
  Docker CLI, or \`/var/run/docker.sock\`.
- **Host-owned keys:** Spawnr forwards SSH signing requests. It does not copy
  private keys into the guest.
- **Versioned runtime:** the CLI downloads a pinned runtime and verifies its
  size, digest, manifest, and files.

## Core commands

| Command | Action |
|---|---|
| `spawnr ls` | List machines |
| `spawnr open <name>` | Open an interactive shell |
| `spawnr start <name>` | Start a stopped machine |
| `spawnr stop <name>` | Stop a machine and keep its data |
| `spawnr publish <name> <oci-ref>` | Publish the environment as an OCI image |
| `spawnr rm <name>` | Remove a machine after a workspace check |

Create a machine without a repository:

```console
$ spawnr init scratch --environment docker.io/library/ubuntu:24.04
$ spawnr start scratch
$ spawnr open scratch
```

Publish environment changes:

```console
$ spawnr publish project-1 ghcr.io/acme/rust-dev:v2
```

## Requirements and current scope

- Linux x86_64.
- Read and write access to \`/dev/kvm\`.
- Linux/amd64 OCI images.
- Outbound networking only.
- No SSH server or IDE Remote SSH integration.
- Publishing requires FUSE and working user namespaces.

Run `spawnr doctor` for a precise host report.

> [!IMPORTANT]
> A forwarded SSH agent does not expose private key bytes. Guest code can still
> request signatures while the machine runs. Read the
> [security model](docs/security.md) before you run untrusted code.

## Develop

The Nix flake pins Rust, Cargo dependencies, Cloud Hypervisor, passt, the guest
kernel, BusyBox, the guest agent, and OCI tools.

```console
$ nix develop
$ cargo test --workspace --locked
$ nix flake check
```

Run the local bundle without replacing an installed release:

```console
$ SPAWNR_HOME=/tmp/spawnr-dev nix run . -- doctor
```

See [CLI reference](docs/cli.md) for names, output formats, data-directory
selection, local OCI layouts, and exact command semantics.

## Environment image contract

Spawnr treats an OCI image as a Linux filesystem distribution, not as a
container application. Its entrypoint and command are not run. Spawnr injects
its static guest agent and BusyBox, then boots the agent as PID 1.

Validated OCI `Config.Env` entries remain the default environment for guest
commands, including image-defined toolchain paths. Spawnr's `dev` identity and
ephemeral Git, GitHub, and SSH session values take precedence. `spawnr open`
uses interactive, non-login `/bin/bash -i`, so interactive image setup belongs
in `~/.bashrc`, not only in a login profile. See
[CLI reference](docs/cli.md#oci-references-and-image-behavior) for the complete
precedence and existing-machine migration rules.

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
- `GH_TOKEN` or `GITHUB_TOKEN`, when set, otherwise the active host `gh auth`
  session when GitHub CLI is available;
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

- [Website and quick start](https://spawnr-cli.dev/)
- [CLI reference](docs/cli.md)
- [Architecture](docs/architecture.md)
- [Security model](docs/security.md)
- [Development and validation](docs/development.md)
- [Release and runtime contract](docs/releases.md)

Spawnr is licensed under [Apache-2.0](LICENSE).
