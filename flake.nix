{
  description = "Spawnr — isolated, reproducible KVM development computers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      lib = pkgs.lib;

      # rust-toolchain.toml is the single source of truth for the compiler,
      # components, and guest musl target used by Cargo and Nix builds.
      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
      muslCC = pkgs.pkgsStatic.stdenv.cc;
      muslTarget = pkgs.pkgsStatic.stdenv.targetPlatform.config;
      muslLinker = "${muslCC}/bin/${muslCC.targetPrefix}cc";

      source = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: type:
          let
            relative = lib.removePrefix (toString ./. + "/") (toString path);
            ignored = name: relative == name || lib.hasPrefix "${name}/" relative;
          in
          !(
            lib.any ignored [
              ".git"
              ".direnv"
              "guest/build"
              "result"
              "target"
            ]
            || lib.hasPrefix "result-" relative
          );
      };

      commonRust = {
        version = "0.1.0";
        src = source;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.pkg-config ];
        strictDeps = true;
        meta = {
          homepage = "https://github.com/spawnr-dev/spawnr";
          license = lib.licenses.asl20;
          platforms = [ system ];
        };
      };

      spawnrHost = rustPlatform.buildRustPackage (
        commonRust
        // {
          pname = "spawnr";
          cargoBuildFlags = [
            "-p"
            "spawnr"
          ];
          cargoTestFlags = [
            "-p"
            "spawnr"
          ];
          postInstall = ''
            test -x "$out/bin/spawnr"
          '';
          meta = commonRust.meta // {
            description = "Host CLI for isolated KVM development computers";
            mainProgram = "spawnr";
          };
        }
      );

      spawnrAgent = rustPlatform.buildRustPackage (
        commonRust
        // {
          pname = "spawnr-agent";
          doCheck = false;
          CARGO_BUILD_TARGET = muslTarget;
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = muslLinker;
          CC_x86_64_unknown_linux_musl = muslLinker;
          buildPhase = ''
            runHook preBuild
            cargo build --frozen --release --target ${muslTarget} -p spawnr-agent
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -Dm755 \
              target/${muslTarget}/release/spawnr-agent \
              "$out/bin/spawnr-agent"
            runHook postInstall
          '';
          postFixup = ''
            if ${pkgs.file}/bin/file "$out/bin/spawnr-agent" | ${pkgs.gnugrep}/bin/grep -q 'dynamically linked'; then
              echo 'spawnr-agent must be statically linked' >&2
              exit 1
            fi
          '';
          meta = commonRust.meta // {
            description = "Static guest PID 1 and control plane for Spawnr";
            mainProgram = "spawnr-agent";
          };
        }
      );

      spawnrStatic = rustPlatform.buildRustPackage (
        commonRust
        // {
          pname = "spawnr-static";
          doCheck = false;
          CARGO_BUILD_TARGET = muslTarget;
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = muslLinker;
          CC_x86_64_unknown_linux_musl = muslLinker;
          buildPhase = ''
            runHook preBuild
            export SPAWNR_RUNTIME_LOCK_JSON="$(cat ${runtimeLockCandidate}/runtime.lock.json)"
            cargo build --frozen --release --target ${muslTarget} -p spawnr
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -Dm755 \
              target/${muslTarget}/release/spawnr \
              "$out/bin/spawnr"
            runHook postInstall
          '';
          postFixup = ''
            if ${pkgs.binutils}/bin/readelf --program-headers "$out/bin/spawnr" \
              | ${pkgs.gnugrep}/bin/grep -q 'Requesting program interpreter'; then
              echo 'release spawnr CLI must not require an ELF interpreter' >&2
              exit 1
            fi
            if ${pkgs.binutils}/bin/readelf --version-info "$out/bin/spawnr" \
              | ${pkgs.gnugrep}/bin/grep -q 'GLIBC_'; then
              echo 'release spawnr CLI must not require glibc symbol versions' >&2
              exit 1
            fi
          '';
          meta = commonRust.meta // {
            description = "Portable static Spawnr release CLI";
            mainProgram = "spawnr";
          };
        }
      );

      runtimeValidator = rustPlatform.buildRustPackage (
        commonRust
        // {
          pname = "spawnr-runtime-validator";
          doCheck = false;
          cargoBuildFlags = [
            "-p"
            "spawnr"
            "--example"
            "validate-runtime"
          ];
          installPhase = ''
            runHook preInstall
            install -Dm755 \
              target/${pkgs.stdenv.hostPlatform.config}/release/examples/validate-runtime \
              "$out/bin/validate-runtime"
            runHook postInstall
          '';
          meta = commonRust.meta // {
            description = "Build-time validator for Spawnr runtime contracts";
          };
        }
      );

      kernel = pkgs.linuxPackages.kernel;
      guestAssets =
        pkgs.runCommand "spawnr-guest-assets-${kernel.modDirVersion}"
          {
            nativeBuildInputs = [
              pkgs.coreutils
              pkgs.cpio
              pkgs.findutils
              pkgs.gnused
              pkgs.gzip
              pkgs.kmod
            ];
          }
          ''
            export SPAWNR_GUEST_KERNEL=${kernel.dev}/vmlinux
            export SPAWNR_GUEST_MODULES=${kernel.modules}/lib/modules/${kernel.modDirVersion}
            export SPAWNR_BUSYBOX=${pkgs.pkgsStatic.busybox}/bin/busybox
            bash ${source}/scripts/build-guest-assets.sh "$out"
          '';

      # Spawnr only accepts registry (docker://) and local OCI-layout sources.
      # Build a pure-Go skopeo without daemon/store transports, GPGME, LVM, or
      # Btrfs support instead of shipping the distribution-oriented wrapper.
      runtimeSkopeo = pkgs.pkgsStatic.buildGoModule {
        pname = "spawnr-runtime-skopeo";
        inherit (pkgs.skopeo) version src;
        vendorHash = null;
        doCheck = false;
        patches = [ ./nix/skopeo-storage-stub.patch ];
        buildPhase = ''
          runHook preBuild
          export CGO_ENABLED=0
          go build \
            -trimpath \
            -tags='exclude_graphdriver_btrfs containers_image_openpgp containers_image_storage_stub containers_image_docker_daemon_stub' \
            -ldflags='-s -w' \
            -o bin/skopeo \
            ./cmd/skopeo
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          install -Dm755 bin/skopeo "$out/bin/skopeo"
          runHook postInstall
        '';
        meta = pkgs.skopeo.meta // {
          description = "Static registry/OCI-only skopeo for the Spawnr runtime";
        };
      };

      sourceMetadata = pkgs.writeText "spawnr-runtime-sources-0.1.0.json" (
        builtins.toJSON {
          schema_version = 1;
          runtime_version = "0.1.0";
          target = "x86_64-linux";
          nixpkgs = {
            revision = nixpkgs.rev;
            nar_hash = nixpkgs.narHash;
          };
          packages = [
            {
              name = "spawnr";
              version = "0.1.0";
              source_url = "https://github.com/spawnr-dev/spawnr/tree/v0.1.0";
              source_hash = "release-tag-and-attestation";
              license_expression = "Apache-2.0";
            }
            {
              name = "busybox";
              version = pkgs.pkgsStatic.busybox.version;
              source_url = "https://busybox.net/downloads/busybox-${pkgs.pkgsStatic.busybox.version}.tar.bz2";
              source_hash = pkgs.pkgsStatic.busybox.src.outputHash;
              license_expression = "GPL-2.0-only";
            }
            {
              name = "ca-certificates";
              version = pkgs.cacert.version;
              source_url = builtins.elemAt pkgs.cacert.src.urls 1;
              source_hash = pkgs.cacert.src.outputHash;
              license_expression = "MPL-2.0";
            }
            {
              name = "cloud-hypervisor";
              version = pkgs.pkgsStatic.cloud-hypervisor.version;
              source_url = pkgs.pkgsStatic.cloud-hypervisor.src.url;
              source_hash = pkgs.pkgsStatic.cloud-hypervisor.src.outputHash;
              license_expression = "Apache-2.0 OR BSD-3-Clause";
            }
            {
              name = "coreutils";
              version = pkgs.pkgsStatic.coreutils.version;
              source_url = "https://ftp.gnu.org/gnu/coreutils/coreutils-${pkgs.pkgsStatic.coreutils.version}.tar.xz";
              source_hash = pkgs.pkgsStatic.coreutils.src.outputHash;
              license_expression = "GPL-3.0-or-later";
            }
            {
              name = "e2fsprogs";
              version = pkgs.pkgsStatic.e2fsprogs.version;
              source_url = "https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v${pkgs.pkgsStatic.e2fsprogs.version}/e2fsprogs-${pkgs.pkgsStatic.e2fsprogs.version}.tar.xz";
              source_hash = pkgs.pkgsStatic.e2fsprogs.src.outputHash;
              license_expression = "NOASSERTION";
              license_summary = "GPL-2.0-or-later, LGPL-2.0-or-later, BSD-3-Clause, and MIT files";
            }
            {
              name = "fuse3";
              version = pkgs.pkgsStatic.fuse3.version;
              source_url = pkgs.pkgsStatic.fuse3.src.url;
              source_hash = pkgs.pkgsStatic.fuse3.src.outputHash;
              license_expression = "GPL-2.0-only AND LGPL-2.1-only";
            }
            {
              name = "linux";
              version = kernel.modDirVersion;
              source_url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${kernel.modDirVersion}.tar.xz";
              source_hash = kernel.src.outputHash;
              license_expression = "GPL-2.0-only";
            }
            {
              name = "passt";
              version = pkgs.pkgsStatic.passt.version;
              source_url = pkgs.pkgsStatic.passt.src.url;
              source_hash = pkgs.pkgsStatic.passt.src.outputHash;
              license_expression = "GPL-2.0-or-later AND BSD-3-Clause";
            }
            {
              name = "skopeo";
              version = runtimeSkopeo.version;
              source_url = runtimeSkopeo.src.url;
              source_hash = runtimeSkopeo.src.outputHash;
              license_expression = "Apache-2.0";
            }
            {
              name = "umoci";
              version = pkgs.pkgsStatic.umoci.version;
              source_url = pkgs.pkgsStatic.umoci.src.url;
              source_hash = pkgs.pkgsStatic.umoci.src.outputHash;
              license_expression = "Apache-2.0";
            }
            {
              name = "util-linux";
              version = pkgs.pkgsStatic.util-linux.version;
              source_url = "https://mirrors.edge.kernel.org/pub/linux/utils/util-linux/v2.42/util-linux-${pkgs.pkgsStatic.util-linux.version}.tar.xz";
              source_hash = pkgs.pkgsStatic.util-linux.src.outputHash;
              license_expression = "NOASSERTION";
              license_summary = "Per-file GPL, LGPL, BSD, MIT, EUPL, and public-domain terms; unshare is GPL-2.0-or-later";
            }
          ];
        }
      );

      runtimeTree =
        let
          static = pkgs.pkgsStatic;
        in
        pkgs.runCommand "spawnr-runtime-tree-0.1.0-x86_64-linux"
          {
            nativeBuildInputs = [
              pkgs.binutils
              pkgs.check-jsonschema
              pkgs.coreutils
              pkgs.file
              pkgs.findutils
              pkgs.gnugrep
              pkgs.jq
              runtimeValidator
            ];
            meta = commonRust.meta // {
              description = "Relocatable static Spawnr runtime tree";
            };
          }
          ''
            mkdir -p "$out/bin" "$out/guest" "$out/share"

            install -m0555 ${static.cloud-hypervisor}/bin/cloud-hypervisor "$out/bin/cloud-hypervisor"
            install -m0555 ${static.passt}/bin/passt "$out/bin/passt"
            install -m0555 ${static.passt}/bin/passt.avx2 "$out/bin/passt.avx2"
            install -m0555 ${runtimeSkopeo}/bin/skopeo "$out/bin/skopeo"
            install -m0555 ${static.umoci}/bin/umoci "$out/bin/umoci"
            install -m0555 ${static.e2fsprogs.bin}/bin/mkfs.ext4 "$out/bin/mkfs.ext4"
            install -m0555 ${static.e2fsprogs.bin}/bin/e2fsck "$out/bin/e2fsck"
            install -m0555 ${static.e2fsprogs.fuse2fs}/bin/fuse2fs "$out/bin/fuse2fs"
            install -m0555 ${static.fuse3}/bin/fusermount3 "$out/bin/fusermount3"
            install -m0555 ${static.util-linux.bin}/bin/unshare "$out/bin/unshare"
            install -m0555 ${static.coreutils}/bin/du "$out/bin/du"

            install -m0644 ${guestAssets}/vmlinux "$out/guest/vmlinux"
            ${pkgs.binutils}/bin/strip --strip-debug "$out/guest/vmlinux"
            chmod 0444 "$out/guest/vmlinux"
            install -m0444 ${guestAssets}/initramfs "$out/guest/initramfs"
            install -m0555 ${static.busybox}/bin/busybox "$out/guest/busybox"
            install -m0555 ${spawnrAgent}/bin/spawnr-agent "$out/guest/spawnr-agent"
            install -m0444 ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt "$out/share/ca-certificates.crt"

            for binary in "$out"/bin/* "$out/guest/busybox" "$out/guest/spawnr-agent"; do
              if ${pkgs.binutils}/bin/readelf --program-headers "$binary" \
                | ${pkgs.gnugrep}/bin/grep -q 'Requesting program interpreter'; then
                echo "runtime executable has an ELF interpreter: $binary" >&2
                exit 1
              fi
              if ${pkgs.binutils}/bin/readelf --version-info "$binary" \
                | ${pkgs.gnugrep}/bin/grep -q 'GLIBC_'; then
                echo "runtime executable requires glibc: $binary" >&2
                exit 1
              fi
            done

            if find "$out" -type l -print -quit | grep -q .; then
              echo 'runtime tree contains a symbolic link' >&2
              exit 1
            fi
            if find "$out" -type f -links +1 -print -quit | grep -q .; then
              echo 'runtime tree contains a hard link' >&2
              exit 1
            fi

            files_json=$TMPDIR/files.json
            printf '[]\n' > "$files_json"
            while IFS= read -r path; do
              relative=''${path#"$out"/}
              size=$(stat --format=%s "$path")
              digest=$(sha256sum "$path" | cut --delimiter=' ' --fields=1)
              executable=false
              if test -x "$path"; then
                executable=true
              fi
              jq \
                --arg path "$relative" \
                --argjson size "$size" \
                --arg digest "$digest" \
                --argjson executable "$executable" \
                '. + [{path: $path, size_bytes: $size, sha256: $digest, executable: $executable}]' \
                "$files_json" > "$files_json.next"
              mv "$files_json.next" "$files_json"
            done < <(find "$out/bin" "$out/guest" "$out/share" -type f -print | LC_ALL=C sort)

            components=$(jq -S -n \
              --arg busybox ${lib.escapeShellArg static.busybox.version} \
              --arg cacert ${lib.escapeShellArg pkgs.cacert.version} \
              --arg cloudHypervisor ${lib.escapeShellArg static.cloud-hypervisor.version} \
              --arg coreutils ${lib.escapeShellArg static.coreutils.version} \
              --arg e2fsprogs ${lib.escapeShellArg static.e2fsprogs.version} \
              --arg fuse3 ${lib.escapeShellArg static.fuse3.version} \
              --arg kernel ${lib.escapeShellArg kernel.modDirVersion} \
              --arg passt ${lib.escapeShellArg static.passt.version} \
              --arg skopeo ${lib.escapeShellArg runtimeSkopeo.version} \
              --arg umoci ${lib.escapeShellArg static.umoci.version} \
              --arg utilLinux ${lib.escapeShellArg static.util-linux.version} \
              '[
                {name: "busybox", version: $busybox, kind: "guest_executable", path: "guest/busybox"},
                {name: "ca-certificates", version: $cacert, kind: "data", path: "share/ca-certificates.crt"},
                {name: "cloud-hypervisor", version: $cloudHypervisor, kind: "host_executable", path: "bin/cloud-hypervisor", launcher: {kind: "direct"}},
                {name: "du", version: $coreutils, kind: "host_executable", path: "bin/du", launcher: {kind: "direct"}},
                {name: "e2fsck", version: $e2fsprogs, kind: "host_executable", path: "bin/e2fsck", launcher: {kind: "direct"}},
                {name: "fuse2fs", version: $e2fsprogs, kind: "host_executable", path: "bin/fuse2fs", launcher: {kind: "direct"}},
                {name: "fusermount3", version: $fuse3, kind: "host_executable", path: "bin/fusermount3", launcher: {kind: "direct"}},
                {name: "guest-initramfs", version: "0.1.0", kind: "guest_initramfs", path: "guest/initramfs"},
                {name: "guest-kernel", version: $kernel, kind: "guest_kernel", path: "guest/vmlinux"},
                {name: "mkfs-ext4", version: $e2fsprogs, kind: "host_executable", path: "bin/mkfs.ext4", launcher: {kind: "direct"}},
                {name: "passt", version: $passt, kind: "host_executable", path: "bin/passt", launcher: {kind: "direct"}},
                {name: "skopeo", version: $skopeo, kind: "host_executable", path: "bin/skopeo", launcher: {kind: "direct"}},
                {name: "spawnr-agent", version: "0.1.0", kind: "guest_executable", path: "guest/spawnr-agent"},
                {name: "umoci", version: $umoci, kind: "host_executable", path: "bin/umoci", launcher: {kind: "direct"}},
                {name: "unshare", version: $utilLinux, kind: "host_executable", path: "bin/unshare", launcher: {kind: "direct"}}
              ]')

            jq -S -n \
              --argjson components "$components" \
              --slurpfile files "$files_json" \
              '{
                schema_version: 1,
                runtime_version: "0.1.0",
                target: "x86_64-linux",
                protocol_version: 1,
                cli_compatibility: {minimum: "0.1.0", maximum_exclusive: "0.2.0"},
                components: $components,
                files: $files[0]
              }' > "$out/manifest.json"

            ${pkgs.check-jsonschema}/bin/check-jsonschema \
              --schemafile ${source}/release/runtime-manifest.schema.json \
              "$out/manifest.json"
            validate-runtime manifest "$out/manifest.json"
            find "$out" -exec touch -h -d '@1' {} +
          '';

      runtimeArchive =
        pkgs.runCommand "spawnr-runtime-0.1.0-x86_64-linux-archive"
          {
            nativeBuildInputs = [
              pkgs.coreutils
              pkgs.findutils
              pkgs.gnutar
              pkgs.jq
              pkgs.zstd
            ];
            meta = commonRust.meta // {
              description = "Deterministic Spawnr runtime release archive";
            };
          }
          ''
            mkdir -p "$out"
            archive_name=spawnr-runtime-0.1.0-x86_64-linux.tar.zst

            make_archive() {
              destination=$1
              (
                cd ${runtimeTree}
                {
                  find bin guest share -type f -print0
                  printf 'manifest.json\0'
                } \
                  | LC_ALL=C sort -z \
                  | tar \
                      --null \
                      --no-recursion \
                      --format=ustar \
                      --numeric-owner \
                      --owner=0 \
                      --group=0 \
                      --mtime='@1' \
                      --mode='u=rX,go=rX' \
                      --no-acls \
                      --no-selinux \
                      --no-xattrs \
                      --create \
                      --file=- \
                      --files-from=-
              ) | zstd -19 --threads=1 --no-progress --stdout > "$destination"
            }

            make_archive "$TMPDIR/runtime-one.tar.zst"
            make_archive "$TMPDIR/runtime-two.tar.zst"
            cmp "$TMPDIR/runtime-one.tar.zst" "$TMPDIR/runtime-two.tar.zst"
            cp "$TMPDIR/runtime-one.tar.zst" "$out/$archive_name"

            archive_sha=$(sha256sum "$out/$archive_name" | cut --delimiter=' ' --fields=1)
            manifest_sha=$(sha256sum ${runtimeTree}/manifest.json | cut --delimiter=' ' --fields=1)
            archive_size=$(stat --format=%s "$out/$archive_name")
            jq -S -n \
              --arg fileName "$archive_name" \
              --arg sha256 "$archive_sha" \
              --arg manifestSha256 "$manifest_sha" \
              --argjson sizeBytes "$archive_size" \
              '{file_name: $fileName, size_bytes: $sizeBytes, sha256: $sha256, manifest_sha256: $manifestSha256}' \
              > "$out/runtime-metadata.json"

            mkdir "$TMPDIR/extracted"
            tar --directory="$TMPDIR/extracted" --extract --file="$out/$archive_name"
            cmp ${runtimeTree}/manifest.json "$TMPDIR/extracted/manifest.json"
            while IFS=$'\t' read -r path expected_size expected_sha; do
              test -f "$TMPDIR/extracted/$path"
              test "$(stat --format=%s "$TMPDIR/extracted/$path")" = "$expected_size"
              test "$(sha256sum "$TMPDIR/extracted/$path" | cut --delimiter=' ' --fields=1)" = "$expected_sha"
            done < <(jq -r '.files[] | [.path, .size_bytes, .sha256] | @tsv' ${runtimeTree}/manifest.json)
          '';

      # This lock is embedded byte-for-byte in the portable CLI. The release
      # transaction will only publish it after independent builders agree on
      # the runtime archive digest.
      runtimeLockCandidate =
        pkgs.runCommand "spawnr-runtime-0.1.0-x86_64-linux-lock-candidate"
          {
            nativeBuildInputs = [
              pkgs.check-jsonschema
              pkgs.jq
              runtimeValidator
            ];
            meta = commonRust.meta // {
              description = "Candidate lock embedded in the portable Spawnr CLI";
            };
          }
          ''
            mkdir -p "$out"
            jq -S -n \
              --slurpfile archive ${runtimeArchive}/runtime-metadata.json \
              '{
                schema_version: 1,
                runtime_version: "0.1.0",
                target: "x86_64-linux",
                protocol_version: 1,
                cli_compatibility: {minimum: "0.1.0", maximum_exclusive: "0.2.0"},
                release_tag: "runtime-v0.1.0",
                archive: {
                  file_name: $archive[0].file_name,
                  format: "tar_zstd",
                  url: ("https://github.com/spawnr-dev/spawnr/releases/download/runtime-v0.1.0/" + $archive[0].file_name),
                  size_bytes: $archive[0].size_bytes,
                  sha256: $archive[0].sha256,
                  manifest_sha256: $archive[0].manifest_sha256
                }
              }' > "$out/runtime.lock.json"
            check-jsonschema \
              --schemafile ${source}/release/runtime-lock.schema.json \
              "$out/runtime.lock.json"
            validate-runtime lock "$out/runtime.lock.json"
          '';

      releaseArtifacts =
        pkgs.runCommand "spawnr-release-artifacts-0.1.0-x86_64-linux"
          {
            nativeBuildInputs = [
              pkgs.coreutils
              pkgs.jq
            ];
            meta = commonRust.meta // {
              description = "Candidate GitHub Release artifacts for Spawnr";
            };
          }
          ''
            mkdir -p "$out"
            export SPAWNR_HOME="$TMPDIR/setup-test"
            runtime_archive=${runtimeArchive}/spawnr-runtime-0.1.0-x86_64-linux.tar.zst
            ${spawnrStatic}/bin/spawnr setup --runtime-archive "$runtime_archive"
            ${spawnrStatic}/bin/spawnr setup --runtime-archive "$runtime_archive" \
              | grep -q 'already installed and verified'
            chmod u+w "$SPAWNR_HOME/runtime/0.1.0/bin/passt"
            printf 'corrupt\n' > "$SPAWNR_HOME/runtime/0.1.0/bin/passt"
            ${spawnrStatic}/bin/spawnr setup --runtime-archive "$runtime_archive"
            cmp \
              ${runtimeTree}/bin/passt \
              "$SPAWNR_HOME/runtime/0.1.0/bin/passt"
            cmp \
              ${runtimeTree}/manifest.json \
              "$SPAWNR_HOME/runtime/0.1.0/manifest.json"

            install -m0555 ${spawnrStatic}/bin/spawnr "$out/spawnr-0.1.0-x86_64-linux"
            install -m0444 \
              ${runtimeArchive}/spawnr-runtime-0.1.0-x86_64-linux.tar.zst \
              "$out/spawnr-runtime-0.1.0-x86_64-linux.tar.zst"
            install -m0444 ${runtimeArchive}/runtime-metadata.json "$out/runtime-metadata.json"
            install -m0444 ${runtimeLockCandidate}/runtime.lock.json "$out/runtime.lock.json"
            install -m0444 ${sourceMetadata} "$out/runtime-sources.json"
            install -m0444 ${source}/LICENSE "$out/LICENSE"

            {
              for license in \
                Apache-2.0 \
                BSD-2-Clause \
                BSD-3-Clause \
                EUPL-1.2 \
                GPL-1.0-or-later \
                GPL-2.0-only \
                GPL-2.0-or-later \
                GPL-3.0-or-later \
                LGPL-2.0-or-later \
                LGPL-2.1-only \
                LGPL-2.1-or-later \
                MIT \
                MPL-2.0; do
                printf '================================================================================\n'
                printf 'SPDX-License-Identifier: %s\n' "$license"
                printf '================================================================================\n\n'
                cat "${pkgs.spdx-license-list-data.text}/text/$license.txt"
                printf '\n\n'
              done
            } > "$out/THIRD-PARTY-LICENSES.txt"

            jq -S -n \
              --slurpfile sources "$out/runtime-sources.json" \
              --slurpfile metadata "$out/runtime-metadata.json" \
              '{
                spdxVersion: "SPDX-2.3",
                dataLicense: "CC0-1.0",
                SPDXID: "SPDXRef-DOCUMENT",
                name: "spawnr-runtime-0.1.0-x86_64-linux",
                documentNamespace: "https://spawnr.dev/sbom/runtime/0.1.0/x86_64-linux",
                creationInfo: {
                  created: "1970-01-01T00:00:01Z",
                  creators: ["Tool: Spawnr-Nix-release-builder"]
                },
                packages: ([{
                  SPDXID: "SPDXRef-Runtime",
                  name: "spawnr-runtime",
                  versionInfo: "0.1.0",
                  downloadLocation: "NOASSERTION",
                  filesAnalyzed: false,
                  licenseConcluded: "NOASSERTION",
                  licenseDeclared: "NOASSERTION",
                  copyrightText: "NOASSERTION",
                  checksums: [{algorithm: "SHA256", checksumValue: $metadata[0].sha256}]
                }] + ($sources[0].packages | map({
                  SPDXID: ("SPDXRef-Package-" + (.name | gsub("[^A-Za-z0-9.-]"; "-"))),
                  name: .name,
                  versionInfo: .version,
                  downloadLocation: .source_url,
                  filesAnalyzed: false,
                  licenseConcluded: "NOASSERTION",
                  licenseDeclared: .license_expression,
                  copyrightText: "NOASSERTION",
                  externalRefs: [{
                    referenceCategory: "OTHER",
                    referenceType: "https://spawnr.dev/spdx/nix-source-hash",
                    referenceLocator: .source_hash
                  }]
                }))),
                relationships: ([{
                  spdxElementId: "SPDXRef-DOCUMENT",
                  relationshipType: "DESCRIBES",
                  relatedSpdxElement: "SPDXRef-Runtime"
                }] + ($sources[0].packages | map({
                  spdxElementId: "SPDXRef-Runtime",
                  relationshipType: "DEPENDS_ON",
                  relatedSpdxElement: ("SPDXRef-Package-" + (.name | gsub("[^A-Za-z0-9.-]"; "-")))
                })))
              }' > "$out/spawnr-runtime-0.1.0-x86_64-linux.spdx.json"

            {
              printf '# Spawnr runtime third-party notices\n\n'
              printf 'Runtime 0.1.0 for x86_64-linux is built from the exact Nix inputs listed below. '
              printf 'NOASSERTION means that licensing is determined per source file; consult the linked source, '
              printf '`THIRD-PARTY-LICENSES.txt`, and the release SBOM.\n\n'
              jq -r '.packages[] | "- **\(.name) \(.version)** — `\(.license_expression)` — [source](\(.source_url))"' \
                "$out/runtime-sources.json"
            } > "$out/THIRD-PARTY-NOTICES.md"

            (
              cd "$out"
              sha256sum \
                LICENSE \
                THIRD-PARTY-LICENSES.txt \
                THIRD-PARTY-NOTICES.md \
                runtime-metadata.json \
                runtime.lock.json \
                runtime-sources.json \
                spawnr-0.1.0-x86_64-linux \
                spawnr-runtime-0.1.0-x86_64-linux.spdx.json \
                spawnr-runtime-0.1.0-x86_64-linux.tar.zst \
                > SHA256SUMS
            )
            chmod 0444 "$out"/*
            chmod 0555 "$out/spawnr-0.1.0-x86_64-linux"
            find "$out" -exec touch -h -d '@1' {} +
          '';

      spawnrBundle =
        pkgs.runCommand "spawnr-bundle-0.1.0"
          {
            nativeBuildInputs = [ pkgs.makeWrapper ];
            meta = commonRust.meta // {
              description = "Runnable Spawnr bundle with pinned host and guest dependencies";
              mainProgram = "spawnr";
            };
          }
          ''
            mkdir -p "$out/bin" "$out/libexec/spawnr" "$out/share/doc/spawnr"
            cp ${spawnrHost}/bin/spawnr "$out/libexec/spawnr/spawnr"
            cp ${spawnrAgent}/bin/spawnr-agent "$out/libexec/spawnr/spawnr-agent"
            cp ${pkgs.pkgsStatic.busybox}/bin/busybox "$out/libexec/spawnr/spawnr-busybox"
            cp ${guestAssets}/vmlinux "$out/libexec/spawnr/vmlinux"
            cp ${guestAssets}/initramfs "$out/libexec/spawnr/initramfs"
            cp ${source}/README.md ${source}/LICENSE "$out/share/doc/spawnr/"

            makeWrapper "$out/libexec/spawnr/spawnr" "$out/bin/spawnr" \
              --set SPAWNR_AGENT "$out/libexec/spawnr/spawnr-agent" \
              --set SPAWNR_BUSYBOX "$out/libexec/spawnr/spawnr-busybox" \
              --set SPAWNR_KERNEL "$out/libexec/spawnr/vmlinux" \
              --set SPAWNR_INITRAMFS "$out/libexec/spawnr/initramfs" \
              --set SPAWNR_CLOUD_HYPERVISOR ${pkgs.cloud-hypervisor}/bin/cloud-hypervisor \
              --set SPAWNR_PASST ${pkgs.passt}/bin/passt \
              --set SPAWNR_SKOPEO ${pkgs.skopeo}/bin/skopeo \
              --set SPAWNR_UMOCI ${pkgs.umoci}/bin/umoci \
              --set SPAWNR_UNSHARE ${pkgs.util-linux}/bin/unshare \
              --set SPAWNR_MKFS_EXT4 ${pkgs.e2fsprogs}/bin/mkfs.ext4 \
              --set SPAWNR_E2FSCK ${pkgs.e2fsprogs}/bin/e2fsck \
              --set SPAWNR_FUSE2FS ${pkgs.e2fsprogs.fuse2fs}/bin/fuse2fs \
              --set SPAWNR_FUSERMOUNT ${pkgs.fuse3}/bin/fusermount3 \
              --set SPAWNR_DU ${pkgs.coreutils}/bin/du
          '';

      workspaceChecks = rustPlatform.buildRustPackage (
        commonRust
        // {
          pname = "spawnr-workspace-checks";
          doCheck = false;
          nativeBuildInputs = commonRust.nativeBuildInputs ++ [
            pkgs.actionlint
            pkgs.python3
            pkgs.shellcheck
          ];
          buildPhase = ''
            runHook preBuild
            actionlint \
              -config-file .github/actionlint.yaml \
              .github/workflows/*.yml
            shellcheck scripts/ci-kvm-release.sh
            python -m py_compile scripts/check-release-tag.py
            cargo fmt --all -- --check
            ${pkgs.check-jsonschema}/bin/check-jsonschema \
              --schemafile release/runtime-lock.schema.json \
              release/runtime.lock.example.json
            ${pkgs.check-jsonschema}/bin/check-jsonschema \
              --schemafile release/runtime-manifest.schema.json \
              release/runtime-manifest.example.json
            cargo test --workspace --frozen
            cargo clippy --workspace --all-targets --frozen -- -D warnings
            runHook postBuild
          '';
          installPhase = ''
            mkdir -p "$out"
            touch "$out/passed"
          '';
        }
      );
    in
    {
      packages.${system} = {
        default = spawnrBundle;
        spawnr = spawnrHost;
        spawnr-static = spawnrStatic;
        spawnr-agent = spawnrAgent;
        guest-assets = guestAssets;
        bundle = spawnrBundle;
        runtime-tree = runtimeTree;
        runtime-archive = runtimeArchive;
        runtime-lock-candidate = runtimeLockCandidate;
        release-artifacts = releaseArtifacts;
      };

      apps.${system}.default = {
        type = "app";
        program = "${spawnrBundle}/bin/spawnr";
        meta.description = "Run Spawnr with all pinned runtime dependencies";
      };

      checks.${system} = {
        workspace = workspaceChecks;
        inherit
          spawnrHost
          spawnrStatic
          spawnrAgent
          guestAssets
          runtimeTree
          runtimeArchive
          runtimeLockCandidate
          releaseArtifacts
          spawnrBundle
          ;
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [
          rustToolchain
          muslCC
          pkgs.cloud-hypervisor
          pkgs.coreutils
          pkgs.cpio
          pkgs.e2fsprogs
          pkgs.e2fsprogs.fuse2fs
          pkgs.file
          pkgs.findutils
          pkgs.fuse3
          pkgs.git
          pkgs.gnumake
          pkgs.gnused
          pkgs.gzip
          pkgs.kmod
          pkgs.passt
          pkgs.pkg-config
          pkgs.pkgsStatic.busybox
          pkgs.bubblewrap
          pkgs.skopeo
          pkgs.umoci
          pkgs.util-linux
        ];
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = muslLinker;
        SPAWNR_GUEST_KERNEL = "${kernel.dev}/vmlinux";
        SPAWNR_GUEST_MODULES = "${kernel.modules}/lib/modules/${kernel.modDirVersion}";
        SPAWNR_BUSYBOX = "${pkgs.pkgsStatic.busybox}/bin/busybox";
        shellHook = ''
          echo 'Spawnr development shell (Rust ${rustToolchain.version}, ${system})'
          echo '  cargo test --workspace --locked'
          echo '  nix build .#bundle'
          echo '  nix flake check'
        '';
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
