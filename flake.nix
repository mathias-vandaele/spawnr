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
          buildPhase = ''
            runHook preBuild
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
        spawnr-agent = spawnrAgent;
        guest-assets = guestAssets;
        bundle = spawnrBundle;
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
          spawnrAgent
          guestAssets
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
