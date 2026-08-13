# Release and runtime contract

Spawnr releases separate the portable CLI, the managed runtime, and host
capabilities. The Nix flake is the build source of truth; GitHub Releases are
the distribution channel for non-Nix installations.

```text
static Spawnr CLI -> pinned runtime archive -> Linux host capabilities
```

This document fixes the V1 contract. The flake constructs candidate release
artifacts; `spawnr setup` and the GitHub Actions publication workflows are
implemented in later milestones.

## Candidate release outputs

The locked flake exposes four non-Nix release layers:

```console
$ nix build .#spawnr-static
$ nix build .#runtime-tree
$ nix build .#runtime-archive
$ nix build .#release-artifacts
```

The release CLI is a stripped static PIE built against musl. The runtime uses
static executables throughout, so V1 does not need its optional ELF-loader
launcher form. Its skopeo build supports registry and OCI-layout transports
but deliberately omits the Docker daemon and containers-storage transports.

`runtime-tree` creates the sorted manifest from the actual files and validates
it with both the public JSON Schema and the Rust semantic contract.
`runtime-archive` normalizes file order, ownership, modes, mtimes, tar format,
and single-threaded zstd compression; it builds the archive twice and requires
byte equality. `release-artifacts` collects the versioned CLI and runtime with
SHA-256 sums, SPDX SBOM, exact source inventory, third-party notices, and
license texts. These are candidates rather than a production release until
the independent-builder and KVM gates below pass.

## Version axes

Three versions change independently:

- the CLI follows the workspace SemVer version;
- the runtime follows its own SemVer version;
- the guest control protocol is the integer `PROTOCOL_VERSION` shared by the
  host and agent crates.

A CLI patch may keep using an existing runtime. For example, CLI `0.1.1` may
continue to use runtime `0.1.0`. A runtime declares a half-open supported CLI
range: `minimum <= CLI < maximum_exclusive`. V1 requires an exact protocol
version match and supports only `x86_64-linux`.

## Contract files

The public schemas and non-deployable examples live in `release/`:

```text
runtime-lock.schema.json
runtime-manifest.schema.json
runtime.lock.example.json
runtime-manifest.example.json
```

The Rust definitions and stricter semantic validation live in
`crates/spawnr/src/runtime.rs`. The examples use `example.invalid` and dummy
digests; no production build may embed or download them.

After the first relocatable archive exists, the release-preparation PR creates
`release/runtime.lock.json`. That real lock is the only file later embedded in
the static release CLI.

Unknown JSON fields are rejected. A schema change that cannot be read safely
by an existing CLI increments `schema_version`.

## Runtime lock

The external lock binds a CLI to one immutable runtime download:

- runtime, target, protocol, and compatible CLI range;
- independent GitHub tag `runtime-v<VERSION>`;
- exact archive file name, HTTPS URL, format, and byte length;
- SHA-256 of the complete archive;
- SHA-256 of the archive's `manifest.json`.

The V1 archive name is:

```text
spawnr-runtime-<VERSION>-x86_64-linux.tar.zst
```

The digest is lowercase hexadecimal and an all-zero placeholder is rejected.
The URL must end in the exact locked file name. A release CLI never resolves
`latest` and never substitutes a distribution package or a binary from
`PATH` for an official runtime component.

## Runtime manifest and archive

The archive root contains `manifest.json` and exactly the regular files listed
by that manifest. Directories are derived from their file paths. V1 archives
contain no symbolic links, hard links, devices, sockets, FIFOs, setuid, or
setgid entries.

Every listed file records its relative path, size, SHA-256, and executable
bit. Paths must:

- be below `bin/`, `guest/`, `lib/`, or `share/`;
- use `/` separators;
- contain no empty, `.`, or `..` component;
- never be absolute.

`manifest.json` does not list or hash itself. Its digest is held by the
external lock, avoiding a self-reference. Components and files are sorted
lexicographically to make generated manifests stable.

Every V1 runtime contains these named components:

```text
busybox             cloud-hypervisor    du
e2fsck              fuse2fs             fusermount3
guest-initramfs     guest-kernel        mkfs-ext4
passt               skopeo              spawnr-agent
umoci               unshare
```

An executable is either launched directly, for a static binary, or through a
bundled ELF loader with bundled library search paths. This allows dynamically
linked implementation tools to remain independent of the host glibc. Guest
executables never use the host launcher declaration.

The archive builder must normalize ordering, ownership, permissions, mtimes,
and compression parameters. Two clean builds of one runtime candidate must
produce the same archive SHA-256.

## Digest and trust chain

The public installer and CLI form this chain:

```text
versioned install.sh
  -> verifies the static CLI SHA-256
  -> CLI contains the runtime lock
  -> verifies the runtime archive SHA-256
  -> verifies manifest.json
  -> verifies every installed runtime file
```

The runtime manifest intentionally does not contain a Git revision. Adding the
new archive digest to `runtime.lock.json` creates a new commit; embedding that
commit in the archive would make the archive digest circular and impossible to
reproduce. Git commit, tag, build inputs, and source provenance are instead
bound externally by the release SBOM and GitHub artifact attestation.

## Release preparation transaction

A runtime-changing release is prepared as a two-commit transaction:

1. Land all code, component, version, and flake changes for the runtime
   candidate.
2. Build the candidate twice from clean Nix builders and require identical
   archive and manifest digests.
3. Commit only the resulting metadata as `release/runtime.lock.json`.
4. Rebuild from that commit and require the same digests.
5. Build the CLI using the committed lock and execute the complete KVM tests
   against those exact artifacts.
6. Tag only the verified commit.

Changing `runtime.lock.json` must not be an input to the runtime archive. A
CLI-only release reuses the existing lock and skips runtime publication.

The release workflow must refuse:

- a tag/version mismatch;
- a missing or modified runtime lock;
- a rebuilt digest different from the committed lock;
- an incompatible CLI or protocol version;
- a manifest containing missing, duplicate, unsorted, unknown, or unsafe
  entries.

## Host boundary

Runtime files are implementation details managed by Spawnr. Kernel-backed or
privileged facilities remain host capabilities and are diagnosed separately:

- Linux x86_64 and KVM access;
- user namespaces and subordinate identity mapping where required;
- FUSE device access where required by publishing.

`spawnr setup` installs files under the user's data directory. It never edits
groups, subordinate-ID configuration, device permissions, sysctls, or kernel
modules.
