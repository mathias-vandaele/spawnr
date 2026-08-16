# Security model

Spawnr is intended to provide a materially stronger boundary for development
and autonomous-agent workloads than executing them directly on the host. Its
primary isolation boundary is a KVM microVM managed by Cloud Hypervisor.

V1 does not claim to resist a KVM, VMM, kernel, or CPU vulnerability, nor does
it make an untrusted guest safe to grant broad credentials. The model is about
minimizing host exposure and making every granted capability explicit.

## Protected assets

Code inside a development VM should not receive ambient access to:

- the host filesystem or `$HOME`;
- `/var/run/docker.sock` or another container/runtime socket;
- private SSH key files;
- unrelated Spawnr machines;
- host processes;
- persistent GitHub credentials; or
- arbitrary host Unix sockets.

Spawnr does not mount host directories into the VM. It does not depend on a
host container daemon. Management runs over AF_VSOCK instead of an IP API.

## Trust boundaries

The host user and Spawnr executable are trusted. The guest filesystem,
repository, package installers, development tools, and code run as `dev` or
root are potentially malicious. KVM, the host kernel, Cloud Hypervisor,
`passt`, `skopeo`, `umoci`, e2fsprogs, FUSE, and their parsers are part of the
trusted computing base for the operations that use them.

The `dev` account has passwordless root inside its own VM. This is intentional:
environment mutation is a product feature. Guest root is not host root.

## Structural storage boundary

| Domain | Guest location | Backing | Lifetime | Publish input |
|---|---|---|---|---|
| Environment | `/` | private raw ext4 `/dev/vda` | persistent | **yes** |
| Workspace | `/workspace` | independent raw ext4 `/dev/vdb` | persistent | **never** |
| Session | `/run/spawnr` | `nosuid,nodev` tmpfs | one boot | **never** |

The guest agent mounts the workspace only from the exact filesystem label
`SPAWNR_WORKSPACE`. It requires `/workspace` to be an exact ext4 mount point
with a different device number from `/`. It also requires `/run/spawnr` to be
an exact tmpfs mount with the expected flags before serving requests.

`/run/spawnr` itself is traversable so guest processes can reach selected
session capabilities. Secret files and the forwarded-agent socket are mode
0600 and owned by `dev`; the security property is the ephemeral tmpfs and
per-capability permissions, not secrecy of the directory name.

On the host, each domain has a marker bound to the machine UUID and workspace
UUID. Publishing validates those markers and passes only `environment.raw` to
`e2fsck`, `fuse2fs`, `umoci`, and `skopeo`. There is no filter intended to
remove `/workspace` from an environment archive: the workspace block device is
absent from the publish pipeline.

This boundary prevents accidental publication of normal workspace and session
state. It cannot prevent a root process in the guest from deliberately copying
a secret or source file from `/workspace` into `/etc`, `/opt`, or another path
on `/`. Anything intentionally written to the environment filesystem is part
of the environment and will be published. Spawnr does not perform secret
scanning.

## Host identity capabilities

Spawnr extracts a bounded subset of host identity instead of sharing whole
configuration directories:

| Capability | Source | Guest representation | Persistence |
|---|---|---|---|
| Git name/email | selected global Git keys | generated Git config | session tmpfs |
| SSH signing key identifier | selected global Git keys | generated Git config | session tmpfs |
| GitHub token | `GH_TOKEN`, `GITHUB_TOKEN`, or active host `gh auth` session | private file and process environment | session tmpfs |
| SSH host keys | system/user known-host files | bounded public record file | session tmpfs |
| SSH signing/authentication | host `SSH_AUTH_SOCK` | Unix socket proxy over vsock | capability while VM runs |

The host's complete `.gitconfig`, credential-helper settings, include paths,
and private key files are not copied. Known-host files contain public server
identity data and are used with strict host-key checking.

SSH-agent forwarding deserves special attention. A forwarded agent does not
reveal private key bytes, but any process in the guest that can reach its Unix
socket can ask the agent to sign. Treat a running VM as having the authority
represented by keys loaded into that agent. Prefer confirmation-constrained or
short-lived agent keys for hostile workloads. Stop the machine to revoke the
forwarded connection.

Likewise, a supplied GitHub token grants its configured scope to guest
processes during that session. Among host token sources, `GH_TOKEN` and
`GITHUB_TOKEN` take precedence. When they are absent and GitHub CLI is
available, Spawnr makes a bounded, non-interactive
`gh auth token --hostname github.com` lookup on the host. It does not copy
GitHub CLI configuration into the guest. Use a narrowly scoped, short-lived
token.

`spawnr open` sets `HISTFILE=/run/spawnr/bash-history`, so ordinary Bash
history expires with the session instead of entering a published environment.
The shell is non-login `/bin/bash -i`; an image-controlled `~/.bashrc` can
override this value after Bash starts. A process can also deliberately copy or
print a credential into an environment-owned file; structural separation
cannot undo that data flow.

## OCI environment variables

OCI `Config.Env` is image metadata, so Spawnr treats it as untrusted, durable
environment defaults rather than host identity or session state. Entries are
read only from the digest-verified copied image, bounded and validated, and
then applied to every guest child after clearing the agent's process
environment. Duplicate names use the OCI runtime convention that the last
entry wins.

Spawnr layers current session controls and the fixed `dev` identity over those
defaults. In particular, image values cannot select `HOME`, `USER`, `LOGNAME`,
or `SHELL`; cannot replace the generated Git configuration; and cannot retain
a baked GitHub-token or SSH-agent control variable when the corresponding host
capability is absent. Explicit operation variables are applied last. These
rules protect Spawnr-owned control names, not arbitrary application variables.
Interactive `.bashrc` code executes afterward and can still alter the shell's
environment.

Never bake a secret into OCI `Config.Env`. It remains visible in image and
cache metadata, survives publishing, and is not protected by session tmpfs.
Filesystem publication preserves the source OCI configuration; exporting a
variable inside a guest does not update that configuration.

## Guest control plane

The agent listens on AF_VSOCK port 19870, not TCP. The host endpoint is a Unix
socket under a mode-0700, UUID-named machine directory. Control messages are
length-prefixed and capped at 8 MiB. Argument vectors and environment maps are
bounded and validated; commands are passed as argv rather than concatenated
into a shell string.

Requested working directories must resolve under `/workspace`; traversal and
symlink escapes are rejected. Captured stdout/stderr are bounded. PTY signal
requests accept only a small signal allowlist.

This control channel trusts callers able to access the host user's private
Spawnr directory. It is not a multi-user authentication boundary.

## Host process and filesystem safety

Spawnr directories are mode 0700 and disk/state files are mode 0600. Machine
paths are derived from UUIDs. Before mutation or recursive removal, Spawnr
checks a regular, private ownership marker against the exact state record.
Existing paths are not adopted or overwritten.

A PID alone is never treated as helper identity. Spawnr records the host boot
ID, kernel process start tick, and executable inode/device, validates them
before signaling, and uses pidfds where available. This prevents a stale PID
file from targeting an unrelated reused process.

Cloud Hypervisor enables Landlock and is given read/write filesystem access to
the machine directory. When agent forwarding is active, the selected
`SSH_AUTH_SOCK` path is added as one explicit rule. The VMM receives no broad
host-home rule.

## Network boundary

`passt` supplies outbound guest networking without a privileged TAP/NAT setup.
Spawnr explicitly disables incoming TCP/UDP port forwarding and passt's special
host-gateway alias. The control plane remains on vsock.

The guest still has Internet egress, and a service deliberately bound to an
externally reachable host address may be reachable according to normal host
routing and firewall rules. `--no-map-gw` is not a substitute for a host
firewall. Egress filtering is not implemented in V1.

## OCI trust and integrity

Spawnr invokes `skopeo` with `--insecure-policy`. That flag bypasses containers
image-signature policy enforcement; it does **not** turn off registry HTTPS or
OCI digest verification. Normal transport TLS remains enabled unless the
user's external registry configuration changes it, and manifests/layers remain
content-addressed.

Consequently, V1 has transport security and content integrity but does not
enforce an image signer/attestation policy. Use only registries and references
you trust, prefer immutable digests for high-risk work, and treat signature
policy support as outstanding work.

Local daemon/store transports (`docker-daemon:`, `containers-storage:`, and
similar) are rejected. Registry traffic and local OCI layouts are handled
directly by `skopeo`; filesystem layer semantics are delegated to `umoci`.
Mutable registry tags are resolved and the subsequent pull is rewritten to
that digest; local OCI tags are verified after copying before a cache is
committed. Publishing disables umoci's default `Config.Volumes` masking so
every path on the environment disk participates in the delta.

The publish E2E unpacks the resulting artifact offline and scans the entire
rootfs for unique workspace and session sentinels. This avoids a false result
from the fresh guest's `/workspace` and `/run` mounts hiding underlying image
paths.

## Data at rest

The environment and workspace images are ordinary private host files, not
encrypted volumes. Host root, the owning host account, host backup software,
or a compromise of that account can read them. Use host full-disk encryption
and normal backup controls where required.

## Residual risks and V1 limits

- Hypervisor/kernel escape vulnerabilities are out of scope.
- Guest workloads can consume CPU, memory, disk capacity, network bandwidth,
  and registry quota; comprehensive resource accounting is not implemented.
- Passwordless guest root can modify any environment state and deliberately
  copy data across the guest's domain mounts.
- Agent and GitHub-token forwarding deliberately give a running guest useful
  host identity authority.
- There is no OCI signature-policy enforcement yet.
- There is no inbound-port feature, SSH service, or multi-user authorization
  layer.
- The fixed VM resource defaults are not a defense against every host denial
  of service.

Report a suspected boundary failure with the exact command, host kernel,
Cloud Hypervisor version, and Spawnr logs from the affected machine's private
session directory. Do not publish or share a disk until potential credential
exposure has been assessed.
