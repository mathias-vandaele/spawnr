# CLI reference

```text
spawnr [--data-dir PATH] [-v...] <command>
```

Global options:

- `--data-dir PATH` selects an absolute Spawnr data root. The same setting is
  available as `SPAWNR_HOME`.
- `-v`, `--verbose` exposes subprocess diagnostics. Repeat up to three times
  for increasing Cloud Hypervisor verbosity.
- `--help` and `--version` print normal Clap help/version output.

Without an override, the data root is `$XDG_DATA_HOME/spawnr`, falling back to
`$HOME/.local/share/spawnr`.

Machine names contain 1–63 lowercase ASCII letters, digits, and interior
hyphens. They must begin and end with a letter or digit.

## `spawnr init`

```text
spawnr init <name> [--environment <oci-reference>]
```

Creates and registers a stopped machine without a repository. The environment
defaults to `docker.io/library/ubuntu:24.04`; the alias `ubuntu` resolves to
the same image.

```console
$ spawnr init scratch
$ spawnr init tools --environment ghcr.io/acme/devtools:2026-08
$ spawnr start scratch
```

Creation resolves and caches the OCI environment, gives the machine an
independent writable environment disk, and creates a fresh workspace disk. It
does not boot the VM.

## `spawnr clone`

```text
spawnr clone <environment> <repository> [--name <name>] [--count <1..100>]
```

Creates a new machine, boots it, exposes the current session capabilities, and
runs `git clone` inside `/workspace`. No host checkout is used.

```console
$ spawnr clone \
    ghcr.io/acme/rust-dev:v4 \
    git@github.com:acme/project.git

$ spawnr clone \
    ghcr.io/acme/rust-dev:v4 \
    https://github.com/acme/project.git \
    --name review --count 5
```

With no explicit name, Spawnr derives a repository slug and allocates the
first unused numeric suffix (`project-1`, `project-2`, ...). `--name review`
is used exactly when count is one; with a larger count it becomes the prefix
(`review-1`, ...). Every instance has an independent environment disk,
workspace disk, VM identity, vsock CID, and MAC address.

The environment must contain Git. An SSH repository URL additionally needs an
SSH client in the image and an appropriate key loaded into the host agent.
Public host-key records are copied from `/etc/ssh/ssh_known_hosts` and
`$HOME/.ssh/known_hosts` into session tmpfs, and strict host-key checking is
used. A clone failure rolls back machines created by that invocation.

## `spawnr start`

```text
spawnr start <name>
```

Boots a stopped machine and waits up to 30 seconds for a healthy guest agent.
Environment and workspace storage are reused. Starting an already running
machine is a successful no-op with a message.

On every start, Spawnr recreates session state and supplies the currently
available Git identity, GitHub token, known-host records, and SSH-agent
capability.

## `spawnr stop`

```text
spawnr stop <name>
```

Requests guest poweroff and then performs bounded VMM/helper cleanup. Both
persistent disks and machine metadata remain. Stopping an already stopped
machine is a successful no-op.

## `spawnr open`

```text
spawnr open <name>
```

Starts the machine if necessary, refreshes session configuration, and opens a
private interactive PTY as `dev`. A repository machine starts in
`/workspace/<repository-directory>`; an initialized machine starts in
`/workspace`.

```console
$ spawnr open project-1
```

The login program is `/bin/bash -l`. The transport is AF_VSOCK via a private
host Unix socket. `open` does not expose SSH or a host network port, and its
interactive lifetime does not retain the global state lock.
Spawnr sets `HISTFILE=/run/spawnr/bash-history` so ordinary Bash history is
session-scoped; image-controlled login startup files can override that value.

## `spawnr publish`

```text
spawnr publish <name> <oci-reference>
```

Publishes the environment filesystem, not the VM. If the VM is running,
Spawnr stops it for a consistent snapshot and starts it again afterward.

```console
$ spawnr publish project-1 ghcr.io/acme/rust-dev:v5
$ spawnr publish project-1 oci:/tmp/rust-dev:v5
```

The first form pushes to a registry. The second writes a local OCI image
layout. Publishing reuses the original OCI layout and adds an `umoci`-generated
filesystem delta. Workspace and session storage are excluded by construction.

Registry references may be written as `ghcr.io/acme/dev:v1` or explicitly as
`docker://ghcr.io/acme/dev:v1`. Here `docker://` is only `skopeo`'s registry
transport name. `docker-daemon:`, `containers-storage:`, `dir:`, `tarball:`,
and `ostree:` are rejected.

Spawnr uses the authentication configuration understood by `skopeo`; it does
not collect a registry password on the command line.

## `spawnr ls`

```text
spawnr ls [--json]
```

Lists only machines recorded in Spawnr state.

```console
$ spawnr ls
NAME       ENVIRONMENT                  REPOSITORY    STATUS
project-1  ghcr.io/acme/rust-dev:v4     acme/project running
scratch    docker.io/library/ubuntu:24.04 -           stopped

$ spawnr ls --json
```

JSON rows contain `name`, `environment`, nullable `repository`, and `status`.
Status is derived from both identity-checked Cloud Hypervisor and passt
processes, not an unverified PID alone. If exactly one helper is live, status
is `degraded`; the next `start` repairs the pair and `stop` always cleans both.

## `spawnr rm`

```text
spawnr rm <name> [--force]
```

Stops the machine and removes its environment, workspace, and runtime state.
Content-addressed base-image cache entries remain available for other machines.

For a machine created by `clone`, the default path starts the VM when needed
and runs `git status --porcelain=v1 --untracked-files=all` inside its workspace.
Spawnr refuses removal when changes are found or cleanliness cannot be proved.
Use `--force` only when discarding the workspace is intentional.

```console
$ spawnr rm project-1
error: Workspace contains uncommitted changes:

  M src/main.rs

Use --force to destroy.

$ spawnr rm project-1 --force
```

Deletion proceeds only after the machine directory's Spawnr ownership marker
matches the state record.

## `spawnr doctor`

```text
spawnr doctor [--json]
```

Performs read-only checks for the current platform, `/dev/kvm`, Cloud
Hypervisor, `passt`, `mkfs.ext4`, the guest kernel, and initramfs. A failed
check prints an actionable remedy and returns a nonzero status.

```console
$ spawnr doctor
$ spawnr doctor --json
```

OCI conversion and publish-only tools are resolved when those operations run;
see [Development](development.md#runtime-dependencies) for the complete list.

## OCI references and image behavior

Spawnr currently selects linux/amd64 when inspecting and copying an image.
Images are filesystem environments: OCI `ENTRYPOINT` and `CMD` are not run.
The injected Spawnr agent is PID 1.

Tags are accepted, but Spawnr records the resolved manifest digest and uses a
content-addressed cache key. A later operation sees a moved tag as new source
content.

The source image must provide `/bin/sh`, `/bin/bash`, and `useradd`. `clone`
additionally requires Git, and SSH Git URLs require an SSH client. Images
without a native `sudo` receive Spawnr's small `sudo COMMAND` fallback;
install real `sudo` in the image when scripts need its broader
option/command-line compatibility.
