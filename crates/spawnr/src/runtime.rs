//! Versioned contract between a Spawnr CLI release and its managed runtime.
//!
//! Downloading and installing the runtime deliberately lives elsewhere. This
//! module defines the signed/hashed data that those operations must accept.

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const RUNTIME_LOCK_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_TARGET: &str = "x86_64-linux";

const REQUIRED_COMPONENTS: &[&str] = &[
    "busybox",
    "cloud-hypervisor",
    "du",
    "e2fsck",
    "fuse2fs",
    "fusermount3",
    "guest-initramfs",
    "guest-kernel",
    "mkfs-ext4",
    "passt",
    "skopeo",
    "spawnr-agent",
    "umoci",
    "unshare",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLock {
    pub schema_version: u32,
    pub runtime_version: String,
    pub target: String,
    pub protocol_version: u16,
    pub cli_compatibility: CliCompatibility,
    pub release_tag: String,
    pub archive: RuntimeArchive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CliCompatibility {
    pub minimum: String,
    pub maximum_exclusive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArchive {
    pub file_name: String,
    pub format: ArchiveFormat,
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    TarZstd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifest {
    pub schema_version: u32,
    pub runtime_version: String,
    pub target: String,
    pub protocol_version: u16,
    pub cli_compatibility: CliCompatibility,
    pub components: Vec<RuntimeComponent>,
    /// Every regular archive member except `manifest.json`.
    pub files: Vec<RuntimeFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeComponent {
    pub name: String,
    pub version: String,
    pub kind: ComponentKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher: Option<RuntimeLauncher>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    HostExecutable,
    GuestKernel,
    GuestInitramfs,
    GuestExecutable,
    Data,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeLauncher {
    Direct,
    ElfLoader {
        loader: String,
        library_paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFile {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub executable: bool,
}

impl RuntimeLock {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(bytes).context("decode Spawnr runtime lock")?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == RUNTIME_LOCK_SCHEMA_VERSION,
            "unsupported runtime lock schema {} (expected {})",
            self.schema_version,
            RUNTIME_LOCK_SCHEMA_VERSION
        );
        let runtime_version = parse_version(&self.runtime_version, "runtime version")?;
        validate_target_protocol(&self.target, self.protocol_version)?;
        self.cli_compatibility.validate()?;
        ensure!(
            self.release_tag == format!("runtime-v{runtime_version}"),
            "runtime release tag must be runtime-v{runtime_version}"
        );

        let expected_file = format!("spawnr-runtime-{runtime_version}-{}.tar.zst", self.target);
        ensure!(
            self.archive.file_name == expected_file,
            "runtime archive must be named {expected_file}"
        );
        ensure!(self.archive.size_bytes > 0, "runtime archive is empty");
        validate_sha256(&self.archive.sha256, "runtime archive SHA-256")?;
        validate_sha256(&self.archive.manifest_sha256, "runtime manifest SHA-256")?;
        validate_download_url(&self.archive.url, &self.archive.file_name)?;
        Ok(())
    }

    pub fn validate_for_cli(&self, cli_version: &Version) -> Result<()> {
        self.validate()?;
        self.cli_compatibility.ensure_contains(cli_version)
    }
}

impl CliCompatibility {
    fn validate(&self) -> Result<()> {
        let minimum = parse_version(&self.minimum, "minimum CLI version")?;
        let maximum = parse_version(&self.maximum_exclusive, "maximum CLI version")?;
        ensure!(
            minimum < maximum,
            "minimum CLI version must be lower than the exclusive maximum"
        );
        Ok(())
    }

    fn ensure_contains(&self, cli_version: &Version) -> Result<()> {
        self.validate()?;
        let minimum = Version::parse(&self.minimum).expect("validated minimum CLI version");
        let maximum =
            Version::parse(&self.maximum_exclusive).expect("validated maximum CLI version");
        ensure!(
            cli_version >= &minimum && cli_version < &maximum,
            "runtime supports CLI versions from {minimum} through {maximum} (exclusive), not {cli_version}"
        );
        Ok(())
    }
}

impl RuntimeManifest {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value: Self =
            serde_json::from_slice(bytes).context("decode Spawnr runtime manifest")?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == RUNTIME_MANIFEST_SCHEMA_VERSION,
            "unsupported runtime manifest schema {} (expected {})",
            self.schema_version,
            RUNTIME_MANIFEST_SCHEMA_VERSION
        );
        parse_version(&self.runtime_version, "runtime version")?;
        validate_target_protocol(&self.target, self.protocol_version)?;
        self.cli_compatibility.validate()?;
        ensure!(!self.components.is_empty(), "runtime has no components");
        ensure!(!self.files.is_empty(), "runtime has no files");

        let mut file_paths = BTreeSet::new();
        let mut previous_file = None;
        for file in &self.files {
            validate_runtime_path(&file.path, false)?;
            ensure!(
                file.path != "manifest.json",
                "manifest.json must not hash itself"
            );
            ensure!(file.size_bytes > 0, "runtime file {} is empty", file.path);
            validate_sha256(&file.sha256, "runtime file SHA-256")?;
            ensure!(
                file_paths.insert(file.path.as_str()),
                "duplicate runtime file {}",
                file.path
            );
            ensure_sorted(previous_file, &file.path, "runtime files")?;
            previous_file = Some(file.path.as_str());
        }

        let mut component_names = BTreeSet::new();
        let mut previous_component = None;
        for component in &self.components {
            validate_component_name(&component.name)?;
            ensure!(
                component_names.insert(component.name.as_str()),
                "duplicate runtime component {}",
                component.name
            );
            ensure_sorted(previous_component, &component.name, "runtime components")?;
            previous_component = Some(component.name.as_str());
            ensure!(
                !component.version.is_empty() && !component.version.chars().any(char::is_control),
                "runtime component {} has an invalid version",
                component.name
            );
            if let Some(expected) = expected_component_kind(&component.name) {
                ensure!(
                    component.kind == expected,
                    "runtime component {} has kind {:?}, expected {:?}",
                    component.name,
                    component.kind,
                    expected
                );
            }
            validate_runtime_path(&component.path, false)?;
            let component_file = self
                .files
                .iter()
                .find(|file| file.path == component.path)
                .with_context(|| {
                    format!(
                        "runtime component {} references an unlisted file {}",
                        component.name, component.path
                    )
                })?;
            validate_component_launcher(component, component_file, &self.files)?;
        }

        for required in REQUIRED_COMPONENTS {
            ensure!(
                component_names.contains(required),
                "runtime is missing required component {required}"
            );
        }
        Ok(())
    }

    pub fn validate_against_lock(&self, lock: &RuntimeLock) -> Result<()> {
        self.validate()?;
        lock.validate()?;
        ensure!(
            self.runtime_version == lock.runtime_version,
            "runtime manifest version does not match its lock"
        );
        ensure!(
            self.target == lock.target,
            "runtime manifest target does not match its lock"
        );
        ensure!(
            self.protocol_version == lock.protocol_version,
            "runtime manifest protocol does not match its lock"
        );
        ensure!(
            self.cli_compatibility == lock.cli_compatibility,
            "runtime manifest CLI compatibility does not match its lock"
        );
        Ok(())
    }
}

fn validate_target_protocol(target: &str, protocol_version: u16) -> Result<()> {
    ensure!(
        target == RUNTIME_TARGET,
        "unsupported runtime target {target:?} (expected {RUNTIME_TARGET})"
    );
    ensure!(
        protocol_version == spawnr_protocol::PROTOCOL_VERSION,
        "runtime protocol {protocol_version} is incompatible with protocol {}",
        spawnr_protocol::PROTOCOL_VERSION
    );
    Ok(())
}

fn validate_component_launcher(
    component: &RuntimeComponent,
    component_file: &RuntimeFile,
    files: &[RuntimeFile],
) -> Result<()> {
    match component.kind {
        ComponentKind::HostExecutable => {
            ensure!(
                component_file.executable,
                "host component {} is not executable",
                component.name
            );
            let launcher = component
                .launcher
                .as_ref()
                .with_context(|| format!("host component {} has no launcher", component.name))?;
            if let RuntimeLauncher::ElfLoader {
                loader,
                library_paths,
            } = launcher
            {
                validate_runtime_path(loader, false)?;
                ensure!(
                    loader.starts_with("lib/"),
                    "ELF loader must be under lib/: {loader}"
                );
                let loader_file = files
                    .iter()
                    .find(|file| file.path == *loader)
                    .with_context(|| format!("ELF loader {loader} is unlisted"))?;
                ensure!(
                    loader_file.executable,
                    "ELF loader {loader} is not executable"
                );
                ensure!(
                    !library_paths.is_empty(),
                    "ELF loader for {} has no library path",
                    component.name
                );
                let mut unique = BTreeSet::new();
                for path in library_paths {
                    validate_runtime_path(path, true)?;
                    ensure!(
                        path == "lib" || path.starts_with("lib/"),
                        "ELF library path must be under lib/: {path}"
                    );
                    ensure!(unique.insert(path), "duplicate ELF library path {path}");
                    let prefix = format!("{path}/");
                    ensure!(
                        files.iter().any(|file| file.path.starts_with(&prefix)),
                        "ELF library path {path} contains no runtime file"
                    );
                }
            }
        }
        ComponentKind::GuestExecutable => {
            ensure!(
                component_file.executable,
                "guest component {} is not executable",
                component.name
            );
            ensure!(
                component.launcher.is_none(),
                "guest component {} must not define a host launcher",
                component.name
            );
        }
        ComponentKind::GuestKernel | ComponentKind::GuestInitramfs | ComponentKind::Data => {
            ensure!(
                component.launcher.is_none(),
                "non-host component {} must not define a launcher",
                component.name
            );
        }
    }
    Ok(())
}

fn parse_version(value: &str, label: &str) -> Result<Version> {
    let version = Version::parse(value).with_context(|| format!("invalid {label} {value:?}"))?;
    ensure!(
        version.build.is_empty(),
        "{label} must not contain build metadata"
    );
    Ok(version)
}

fn expected_component_kind(name: &str) -> Option<ComponentKind> {
    Some(match name {
        "busybox" | "spawnr-agent" => ComponentKind::GuestExecutable,
        "guest-initramfs" => ComponentKind::GuestInitramfs,
        "guest-kernel" => ComponentKind::GuestKernel,
        "cloud-hypervisor" | "du" | "e2fsck" | "fuse2fs" | "fusermount3" | "mkfs-ext4"
        | "passt" | "skopeo" | "umoci" | "unshare" => ComponentKind::HostExecutable,
        _ => return None,
    })
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} must be 64 lowercase hexadecimal characters"
    );
    ensure!(
        !value.bytes().all(|byte| byte == b'0'),
        "{label} must not be the all-zero placeholder"
    );
    Ok(())
}

fn validate_download_url(url: &str, file_name: &str) -> Result<()> {
    ensure!(url.starts_with("https://"), "runtime URL must use HTTPS");
    ensure!(
        !url.chars().any(char::is_whitespace) && !url.contains('#'),
        "runtime URL contains invalid characters"
    );
    let remainder = url
        .strip_prefix("https://")
        .expect("validated HTTPS prefix");
    let (authority, path) = remainder
        .split_once('/')
        .context("runtime URL must contain a host and path")?;
    ensure!(
        !authority.is_empty() && !authority.contains('@') && !path.is_empty(),
        "runtime URL must contain a host without user information and a path"
    );
    ensure!(
        url.ends_with(&format!("/{file_name}")),
        "runtime URL must end with its archive file name"
    );
    Ok(())
}

fn validate_component_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| { byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' })
            && !name.starts_with('-')
            && !name.ends_with('-'),
        "invalid runtime component name {name:?}"
    );
    Ok(())
}

fn validate_runtime_path(path: &str, directory: bool) -> Result<()> {
    ensure!(!path.is_empty(), "runtime path is empty");
    ensure!(
        !path.starts_with('/'),
        "runtime path must be relative: {path}"
    );
    ensure!(
        !path.contains('\\'),
        "runtime path contains a backslash: {path}"
    );
    ensure!(
        !path.chars().any(char::is_control),
        "runtime path contains a control character"
    );
    ensure!(
        path.split('/')
            .all(|component| !component.is_empty() && component != "." && component != ".."),
        "runtime path contains an unsafe component: {path}"
    );
    let top = path.split('/').next().expect("non-empty runtime path");
    ensure!(
        matches!(top, "bin" | "guest" | "lib" | "share"),
        "runtime path is outside the allowed layout: {path}"
    );
    if directory {
        ensure!(
            !path.ends_with('/'),
            "runtime directory must omit trailing slash"
        );
    } else {
        ensure!(
            path.contains('/'),
            "runtime file path must include a file name: {path}"
        );
    }
    Ok(())
}

fn ensure_sorted(previous: Option<&str>, current: &str, label: &str) -> Result<()> {
    if let Some(previous) = previous {
        ensure!(
            previous < current,
            "{label} must be sorted lexicographically"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &[u8] = include_bytes!("../../../release/runtime.lock.example.json");
    const MANIFEST: &[u8] = include_bytes!("../../../release/runtime-manifest.example.json");

    #[test]
    fn example_contract_is_valid_and_compatible() {
        let lock = RuntimeLock::from_json(LOCK).unwrap();
        let manifest = RuntimeManifest::from_json(MANIFEST).unwrap();
        manifest.validate_against_lock(&lock).unwrap();
        let cli_version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        lock.validate_for_cli(&cli_version).unwrap();
    }

    #[test]
    fn runtime_lock_rejects_placeholder_digest() {
        let mut lock = RuntimeLock::from_json(LOCK).unwrap();
        lock.archive.sha256 = "0".repeat(64);
        assert!(
            lock.validate()
                .unwrap_err()
                .to_string()
                .contains("placeholder")
        );
    }

    #[test]
    fn manifest_rejects_unsafe_paths() {
        let mut manifest = RuntimeManifest::from_json(MANIFEST).unwrap();
        manifest.files[0].path = "bin/../escape".into();
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unsafe")
        );
    }

    #[test]
    fn manifest_must_match_its_lock() {
        let lock = RuntimeLock::from_json(LOCK).unwrap();
        let mut manifest = RuntimeManifest::from_json(MANIFEST).unwrap();
        manifest.runtime_version = "0.1.1".into();
        assert!(
            manifest
                .validate_against_lock(&lock)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
    }

    #[test]
    fn manifest_requires_every_v1_component() {
        let mut manifest = RuntimeManifest::from_json(MANIFEST).unwrap();
        manifest
            .components
            .retain(|component| component.name != "passt");
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("missing required component passt")
        );
    }

    #[test]
    fn required_component_kinds_are_fixed() {
        let mut manifest = RuntimeManifest::from_json(MANIFEST).unwrap();
        manifest
            .components
            .iter_mut()
            .find(|component| component.name == "guest-kernel")
            .unwrap()
            .kind = ComponentKind::Data;
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("expected GuestKernel")
        );
    }
}
