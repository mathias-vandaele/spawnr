# Release and runtime contract

Spawnr releases separate the portable CLI, the managed runtime, and host
capabilities. The Nix flake is the build source of truth; GitHub Releases are
the distribution channel for non-Nix installations.

```text
static Spawnr CLI -> pinned runtime archive -> Linux host capabilities
```

This document fixes the V1 contract. The flake constructs candidate release
artifacts and `spawnr setup` installs them; GitHub Actions publication is a
separate release milestone.

## Candidate release outputs

The locked flake exposes these non-Nix release layers:

```console
$ nix build .#spawnr-static
$ nix build .#runtime-tree
$ nix build .#runtime-archive
$ nix build .#runtime-lock-candidate
$ nix build .#installer
$ nix build .#native-packages
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

`native-packages` derives a Debian package, an RPM package, and the AUR
`spawnr-bin` metadata from the exact `spawnr-static` output. Debian and RPM
contain `/usr/bin/spawnr`, the project licence, and README only. They declare
no runtime dependency and contain no maintainer or transaction scripts. The
AUR recipe downloads the immutable versioned CLI and licence assets, verifies
both SHA-256 digests, disables stripping so the installed CLI remains
byte-identical, and likewise performs no setup automatically.

## Version axes

Three versions change independently:

- the CLI follows the workspace SemVer version;
- the runtime follows its own SemVer version;
- the guest control protocol is the integer `PROTOCOL_VERSION` shared by the
  host and agent crates.

The guest agent and protocol crates keep explicit package versions instead of
inheriting the CLI workspace version. Therefore a CLI-only version bump does
not rewrite runtime component metadata; an agent, boot, kernel, or bundled-tool
change still requires a new runtime version and lock.

A CLI patch may keep using an existing runtime. For example, CLI `0.1.1` may
continue to use runtime `0.1.0`. A runtime declares a half-open supported CLI
range: `minimum <= CLI < maximum_exclusive`. V1 requires an exact protocol
version match and supports only `x86_64-linux`.

`release/config.toml` is the source of truth for the public GitHub repository,
website, target, runtime version, and compatible CLI interval. `Cargo.toml`
remains the source of truth for the CLI version. The flake reads both files;
the release preflight rejects a disagreement instead of publishing internally
consistent artifacts at the wrong repository or URL.

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

The flake derives `runtime-lock-candidate` from the relocatable archive and
embeds that exact JSON in the candidate static CLI, allowing the complete
setup flow to be tested before publication. The release transaction promotes
identical independently reproduced metadata to `release/runtime.lock.json`;
the publication workflow must reject any difference.

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
`PATH` for an official runtime component. Explicit `SPAWNR_*` overrides remain
available for development and diagnosis. Nix builds intentionally have no
embedded public-release lock and resolve the components supplied by their Nix
wrapper.

## Managed installation

For a public release, the normal flow is:

```console
$ spawnr setup
$ spawnr doctor
```

`setup` streams the lock's HTTPS archive into a private temporary file with a
finite timeout and exact byte limit. Before activation it verifies the archive
digest, accepts only normalized regular tar members, validates the manifest
against the external lock, and verifies every file's path, size, mode, and
SHA-256. The versioned directory and `active.json` pointer are committed
atomically while termination signals are blocked. Repeating setup is
idempotent; repeating it after local corruption replaces that same version
atomically.

Air-gapped and release validation use the same code path:

```console
$ spawnr setup \
    --runtime-lock ./runtime.lock.json \
    --runtime-archive ./spawnr-runtime-0.1.0-x86_64-linux.tar.zst
```

The archive must still match the selected or embedded lock exactly.

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

## GitHub Actions release gates

`.github/workflows/ci.yml` runs the complete flake check for every branch and
pull request with read-only repository access. `.github/workflows/release.yml`
can be exercised without publication through `workflow_dispatch`; pushing a
`runtime-v*` or `v*` tag uses the same gates and enables the final publication
job.

The release workflow is deliberately split across security boundaries:

1. A cheap preflight requires `GITHUB_REPOSITORY` to equal the public identity
   in `release/config.toml` before any expensive build starts.
2. Two fresh `ubuntu-24.04` jobs independently build `release-artifacts`.
3. A third job verifies both checksum sets and requires recursive byte
   equality. For a tag, it also binds runtime tags to the generated lock and
   CLI tags to the Cargo version plus committed runtime lock.
4. A self-hosted runner carrying the labels `linux`, `x64`, and `spawnr-kvm`
   installs the exact verified archive with the exact static CLI, runs
   `doctor`, then executes the critical clone/open/crash/restart/publish/
   reimport/rollback KVM scenario.
5. Only the final `release` environment job receives `contents: write`, OIDC,
   and attestation permissions. It reverifies checksums, creates provenance
   attestations, selects only the assets belonging to that runtime or CLI
   channel, uploads them to a draft release, and publishes the draft only
   after every upload succeeds.
6. A successful CLI release deploys its checksum-pinned `install.sh` to
   GitHub Pages at `spawnr-cli.dev`; runtime-only releases never alter the public
   installer channel.
7. The final job downloads that public URL, requires it to be byte-identical
   to the verified candidate, runs the same installer a user receives, checks
   the installed version, executes `spawnr setup` twice, and requires
   `spawnr doctor --json` to report a healthy managed runtime.
   Host KVM may be unavailable on this smoke runner; the protected KVM job is
   authoritative for virtualization.

The CLI release also publishes the independently reproduced `.deb`, `.rpm`,
`PKGBUILD`, and `.SRCINFO` from the same candidate. Publishing the AUR metadata
to the separate AUR Git repository remains an explicit maintainer operation;
the GitHub workflow does not hold an AUR SSH key.

Every referenced action is pinned to a complete commit SHA. No release job
uses Docker, a Docker socket, a registry password, or a long-lived publishing
secret; publication uses the job-scoped GitHub token.

Repository configuration is part of the release boundary and cannot be
expressed fully in source:

- host or transfer the project at the exact repository named by
  `release/config.toml` (`spawnr-dev/spawnr` for V1); the workflow refuses to
  publish from a personal fork or differently named repository;
- enable GitHub immutable releases before the first public release;
- configure GitHub Pages to deploy through Actions and verify the
  `spawnr-cli.dev` custom domain in repository settings;
- protect `v*` and `runtime-v*` tag creation;
- configure `kvm-release` and `release` environments with required reviewers
  and restrict them to protected release tags (the first protects access to
  the isolated virtualization runner; the second protects publication);
- attach an ephemeral or tightly isolated x86_64 Linux runner with KVM, FUSE,
  user namespaces, subordinate UID/GID mappings, and the `spawnr-kvm` label;
- do not expose unrelated repository or organization secrets to that runner.

The KVM fixture is a public, digest-pinned `linux/amd64` development image, so
the gate does not depend on mutable OCI tags.

For an Actions-based Pages deployment, the custom domain is configured in
GitHub repository settings and DNS, not through a generated `CNAME` file. DNS
and certificate provisioning must be complete before the first CLI tag, or
the post-release public installer smoke will fail visibly.

### One-time GitHub Pages and DNS setup

Perform these steps in order after the canonical repository exists at
`spawnr-dev/spawnr`:

1. In the `spawnr-dev` organization settings, open **Pages**, add
   `spawnr-cli.dev` as a verified domain, and publish the TXT challenge that
   GitHub provides. Keep that TXT record after verification; it protects the
   domain from being claimed by another GitHub account.
2. In the repository's **Settings → Pages**, select **GitHub Actions** as the
   source, enter `spawnr-cli.dev` as the custom domain, and save it before
   changing the traffic records at the registrar.
3. At the DNS provider, point the apex to GitHub Pages with these four `A`
   records: `185.199.108.153`, `185.199.109.153`, `185.199.110.153`, and
   `185.199.111.153`. The equivalent GitHub `AAAA` records may be added for
   IPv6. Optionally point `www` by `CNAME` to `spawnr-dev.github.io`.
4. Do not create wildcard records such as `*.spawnr-cli.dev`. Wait for GitHub's
   DNS check and certificate issuance, then enable **Enforce HTTPS**.

The release workflow deploys the complete static site, its public schemas, and
the checksum-pinned installer as one Pages artifact. A runtime-only tag does
not modify the site. The first site deployment therefore happens with the
first successful CLI release; until then the domain can be configured without
serving an unverified installer.

## First public release preflight

Before creating any public tag, build the candidate from a clean checkout and
run the same binding check used by GitHub Actions:

```console
$ nix flake check
$ nix build .#release-artifacts
$ scripts/release-preflight.py candidate runtime-v0.1.0 result \
    --check-remote --require-clean
```

The preflight verifies the configured public identity, local `origin`, clean
worktree, complete SHA-256 set, runtime archive against its lock, release URL,
and tag/version binding. For a CLI tag, use `v<CLI_VERSION>`; it additionally
requires the exact independently published lock at
`release/runtime.lock.json`, and verifies the CLI, installer, Debian, RPM, and
AUR versions and digests.

The first release is intentionally blocked until all of these external items
are true:

- the canonical repository is `spawnr-dev/spawnr` and local `origin` points
  to it;
- Actions Pages serves `https://spawnr-cli.dev` with HTTPS enforced;
- immutable releases and protected `v*`/`runtime-v*` tags are enabled;
- `kvm-release` and `release` environments require approval;
- the isolated `spawnr-kvm` runner is online and has no unrelated secrets;
- `release/runtime.lock.json` is absent for the first runtime tag, then added
  only from the independently reproduced immutable runtime release before the
  CLI tag.

## Public release sequence

For a runtime-changing release:

1. Run the Release workflow manually on the intended commit. This performs
   both independent builds and the exact-artifact KVM gate without publishing.
2. Create and push `runtime-v<RUNTIME_VERSION>`. After approval, the workflow
   publishes the canonical runtime archive and its candidate lock.
3. Download `runtime.lock.json` from that immutable runtime release, verify it
   equals `nix build .#runtime-lock-candidate`, and commit it as
   `release/runtime.lock.json` without changing runtime inputs.
4. Re-run all checks, create `v<CLI_VERSION>` on that commit, and approve the
   user-facing release. The release preflight refuses publication unless the
   committed lock exactly equals the independently reproduced candidate.

For a CLI-only patch, keep `release/runtime.lock.json` unchanged and create the
new CLI tag after the normal candidate and KVM gates.

Tags are never reused. If a public smoke or user-visible behavior fails after
publication, fix the channel or code and publish a new patch version; do not
replace an immutable asset.

## Bootstrap installer

`install/install.sh.in` is a source template, not a mutable downloader. Nix
generates the release's `install.sh` after the static CLI exists and embeds
that exact binary's byte size and SHA-256 plus the exact `v<CLI_VERSION>`
GitHub URL. The installer accepts only Linux x86_64, requires HTTPS/TLS 1.2 or
newer, limits and verifies the download, then atomically installs `spawnr` to
`$SPAWNR_INSTALL_DIR` or `$HOME/.local/bin`.

It intentionally does not download the much larger runtime while executing
inside `curl | sh`. Its complete success output points to the auditable second
stage:

```console
$ spawnr setup
$ spawnr doctor
```

Updating `https://spawnr-cli.dev/install.sh` therefore requires a fully gated CLI
release. The generic URL never resolves GitHub's mutable `latest` alias.

## Upgrade, rollback, and removal

Upgrade uses the same public entry point as first installation:

```console
$ curl -fsSL https://spawnr-cli.dev/install.sh | sh
$ spawnr setup
$ spawnr doctor
```

The installer atomically replaces only the CLI. `setup` then installs and
activates the exact runtime embedded in that CLI; versioned older runtime
directories are retained. Native-package users upgrade through their package
manager and run the same explicit `setup` and `doctor` stages.

Rollback is an explicit version choice, never a mutable `latest` lookup. Stop
machines first, preserve the Spawnr data directory, download `install.sh` from
the desired immutable `v<VERSION>` GitHub release, inspect it, run it, then run
that CLI's `setup` and `doctor`. A rollback is supported only when the older
CLI understands the existing state and image formats; otherwise restore the
matching data backup or move forward with a patch release.

Removal is deliberately not hidden inside the network installer. Stop and
remove machines through Spawnr, remove the single installed CLI from the
chosen installation directory, and remove the Spawnr data directory only if
the user explicitly intends to destroy cached OCI data, runtimes, disks, and
machine state.

## Native packages

The native packages are a convenience layer over the portable CLI, not a
second runtime policy. They never install Cloud Hypervisor, `passt`, guest
assets, OCI tools, or a Docker daemon. After installing a `.deb`, `.rpm`, or
the AUR `spawnr-bin` package, users run the same explicit and auditable flow:

```console
$ spawnr setup
$ spawnr doctor
```

Nix builds both native archives twice with `SOURCE_DATE_EPOCH` fixed, requires
byte equality, extracts each package, compares its executable with
`spawnr-static`, executes its version command, and rejects package transaction
scripts. It also regenerates `.SRCINFO` from `PKGBUILD` and requires byte
equality with the published metadata.

## Host boundary

Runtime files are implementation details managed by Spawnr. Kernel-backed or
privileged facilities remain host capabilities and are diagnosed separately:

- Linux x86_64 and KVM access;
- user namespaces and subordinate identity mapping where required;
- FUSE device access where required by publishing.

`spawnr setup` installs files under the user's data directory. It never edits
groups, subordinate-ID configuration, device permissions, sysctls, or kernel
modules.
