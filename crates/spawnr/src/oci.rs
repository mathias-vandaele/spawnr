//! OCI environment pull, materialization, and publishing.
//!
//! Spawnr deliberately uses the mature, daemonless `skopeo` and `umoci`
//! implementations of OCI Distribution and filesystem-layer semantics. No
//! Docker daemon, Docker CLI, container runtime socket, or host checkout is
//! involved. `umoci repack` owns diff/whiteout generation; Spawnr never tries
//! to approximate OCI deletion semantics itself.

use crate::paths::{Paths, create_private_dir};
use crate::process;
use crate::state::MachineRecord;
use crate::storage::{MachinePaths, validate_ext4_image};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use uuid::Uuid;

const DEFAULT_ENVIRONMENT_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const CACHE_METADATA: &str = "spawnr-environment.json";
const CACHE_DISK: &str = "environment.raw";
const MAX_GUEST_ENVIRONMENT_ITEMS: usize = 512;
const MAX_GUEST_ENVIRONMENT_BYTES: usize = 1024 * 1024;
// JSON escaping can expand a byte to a six-byte `\u00xx` sequence. Keep both
// subprocess output and cache-metadata decoding bounded while still admitting
// every environment which satisfies MAX_GUEST_ENVIRONMENT_BYTES.
const MAX_GUEST_ENVIRONMENT_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_SUBPROCESS_DIAGNOSTIC_BYTES: usize = 1024 * 1024;
const INTEGRATION_CACHE_SCHEMA: &[u8] = b"spawnr-guest-integration-v3";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedMetadata {
    pub source_reference: String,
    pub source_digest: String,
    pub cache_key: String,
    pub architecture: String,
    pub os: String,
    /// Exact OCI Config.Env entries. Ordering is retained so duplicate names
    /// keep the OCI runtime's last-entry-wins behavior.
    #[serde(default)]
    pub config_env: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Tools {
    skopeo: PathBuf,
    ca_certificates: Option<PathBuf>,
    umoci: PathBuf,
    unshare: PathBuf,
    mkfs_ext4: PathBuf,
    du: PathBuf,
    agent: PathBuf,
    busybox: PathBuf,
}

#[derive(Debug, Clone)]
struct PublishTools {
    common: Tools,
    e2fsck: PathBuf,
    fuse2fs: PathBuf,
    fusermount: PathBuf,
}

/// Resolve, pull, unpack, provision, and cache an OCI environment as a raw
/// ext4 image. The returned cache image is immutable by convention and never
/// attached to a VM; storage.rs always reflink/copies it first.
pub fn prepare_environment(paths: &Paths, record: &MachineRecord, verbose: u8) -> Result<PathBuf> {
    ensure!(env::consts::OS == "linux" && env::consts::ARCH == "x86_64");
    paths
        .ensure_layout()
        .context("create private OCI cache directories")?;
    let tools = Tools::discover(paths).context("locate daemonless OCI and guest tools")?;
    let reference = normalize_source_reference(&record.environment.reference)
        .context("validate OCI environment reference")?;
    let digest =
        inspect_digest(&tools, &reference, verbose).context("resolve OCI environment manifest")?;
    let cache_key = integration_cache_key(&tools, &digest)
        .context("identify OCI base plus Spawnr guest integration")?;
    let cache_key = cache_key.as_str();
    ensure_hex_digest(cache_key).context("validate OCI manifest digest")?;
    let cache = paths.images_dir().join(cache_key);
    let disk = cache.join(CACHE_DISK);
    if valid_cache(&cache, &disk, &digest, cache_key).context("inspect OCI environment cache")? {
        return Ok(disk);
    }

    let lock_path = paths.images_dir().join(format!("{cache_key}.lock"));
    let _lock = CacheLock::acquire(&lock_path).context("lock OCI environment cache entry")?;
    if valid_cache(&cache, &disk, &digest, cache_key).context("reinspect locked OCI cache entry")? {
        return Ok(disk);
    }
    if cache.exists() {
        bail!(
            "OCI cache entry {} is incomplete; remove it after confirming no Spawnr operation is running",
            cache.display()
        );
    }

    let temporary = paths
        .images_dir()
        .join(format!(".{cache_key}.{}.tmp", Uuid::new_v4()));
    create_private_dir(&temporary).context("create temporary OCI cache entry")?;
    let result = build_cache(
        &tools,
        &temporary,
        &reference,
        &record.environment.reference,
        &digest,
        cache_key,
        verbose,
    )
    .context("build content-addressed OCI environment cache");
    if let Err(error) = result {
        remove_tree_mapped(&tools, &temporary).ok();
        return Err(error);
    }
    fs::rename(&temporary, &cache).with_context(|| {
        format!(
            "atomically install OCI environment cache {}",
            cache.display()
        )
    })?;
    sync_directory(&paths.images_dir()).context("sync OCI cache directory")?;
    Ok(disk)
}

pub fn prepared_metadata(disk: &Path) -> Result<PreparedMetadata> {
    let directory = disk
        .parent()
        .context("prepared environment has no cache directory")?;
    let metadata = read_prepared_metadata(&directory.join(CACHE_METADATA))
        .context("read prepared environment metadata")?;
    validate_prepared_metadata(&metadata, true)?;
    Ok(metadata)
}

/// Return the immutable source image environment bound to a machine's cache.
/// Older machines created before Config.Env support intentionally receive an
/// empty baseline instead of becoming unbootable after a CLI upgrade.
pub fn machine_config_env(paths: &Paths, record: &MachineRecord) -> Result<Vec<String>> {
    let cache_key = record
        .environment
        .base_cache_key
        .as_deref()
        .context("machine metadata has no immutable OCI base cache identity")?;
    ensure_hex_digest(cache_key)?;
    let metadata = read_prepared_metadata(&paths.images_dir().join(cache_key).join(CACHE_METADATA))
        .context("read OCI base metadata")?;
    validate_prepared_metadata(&metadata, false)?;
    ensure!(
        metadata.cache_key == cache_key,
        "machine OCI cache metadata has the wrong identity"
    );
    if let Some(expected) = &record.environment.manifest_digest {
        ensure!(
            expected == &metadata.source_digest,
            "machine OCI digest does not match its cache metadata"
        );
    }
    let environment = metadata.config_env.unwrap_or_default();
    validate_image_environment(&environment)?;
    Ok(environment)
}

/// Publish only the environment block device. The workspace disk and session
/// directory are neither mounted nor passed to any publishing subprocess.
pub fn publish_machine_environment(
    paths: &Paths,
    record: &MachineRecord,
    reference: &str,
    verbose: u8,
) -> Result<()> {
    let tools = PublishTools::discover(paths)?;
    let machine = MachinePaths::for_record(paths, record);
    machine.assert_domain_layout(record)?;
    validate_ext4_image(&machine.environment_disk)?;

    let cache_key = record
        .environment
        .base_cache_key
        .as_deref()
        .context("machine metadata has no immutable OCI base cache identity")?;
    ensure_hex_digest(cache_key)?;
    let cache = paths.images_dir().join(cache_key);
    let metadata =
        read_prepared_metadata(&cache.join(CACHE_METADATA)).context("read OCI base metadata")?;
    validate_prepared_metadata(&metadata, false)?;
    if let Some(expected) = &record.environment.manifest_digest {
        ensure!(
            expected == &metadata.source_digest,
            "machine OCI digest does not match its cache metadata"
        );
    }

    run_e2fsck(&tools.e2fsck, &machine.environment_disk, verbose)
        .context("environment filesystem is inconsistent; refusing to publish")?;
    let target = normalize_destination_reference(reference)?;
    let temporary = paths.oci_dir().join(format!("publish-{}", Uuid::new_v4()));
    create_private_dir(&temporary)?;
    let result = (|| -> Result<()> {
        copy_tree(&cache.join("layout"), &temporary.join("layout"))?;
        let bundle = temporary.join("bundle");
        let script = temporary.join("repack.sh");
        write_executable(&script, REPACK_SCRIPT.as_bytes())?;
        let status = Command::new(&tools.common.unshare)
            .args([
                "--user",
                "--map-auto",
                "--map-root-user",
                "--mount",
                "--fork",
                "--",
            ])
            .arg(&script)
            .arg(&tools.common.umoci)
            .arg(&tools.fuse2fs)
            .arg(&tools.fusermount)
            .arg(temporary.join("layout"))
            .arg(&machine.environment_disk)
            .arg(&bundle)
            .status()
            .context("generate OCI environment delta in a user namespace")?;
        ensure!(
            status.success(),
            "umoci failed to generate environment layer ({status})"
        );

        let mut command = skopeo_command(&tools.common);
        command
            .arg("--insecure-policy")
            .arg("copy")
            .args(["--format", "oci", "--dest-precompute-digests"])
            .arg(format!(
                "oci:{}:published",
                temporary.join("layout").display()
            ))
            .arg(&target);
        run_checked(&mut command, verbose, "push published OCI environment")?;
        Ok(())
    })();
    remove_tree_mapped(&tools.common, &temporary).ok();
    result
}

fn build_cache(
    tools: &Tools,
    temporary: &Path,
    transport_reference: &str,
    user_reference: &str,
    digest: &str,
    cache_key: &str,
    verbose: u8,
) -> Result<()> {
    let layout = temporary.join("layout");
    let bundle = temporary.join("bundle");
    let pull_reference = immutable_pull_reference(transport_reference, digest)?;
    let mut pull = skopeo_command(tools);
    pull.arg("--insecure-policy")
        .arg("copy")
        .args(["--override-os", "linux", "--override-arch", "amd64"])
        .arg(&pull_reference)
        .arg(format!("oci:{}:base", layout.display()));
    run_checked(&mut pull, verbose, "pull OCI environment")?;
    if transport_reference.starts_with("oci:") {
        let copied = inspect_digest(tools, &format!("oci:{}:base", layout.display()), verbose)?;
        ensure!(
            copied == digest,
            "local OCI tag changed while it was being copied (expected {digest}, copied {copied}); retry"
        );
    }
    let image_environment =
        inspect_image_environment(tools, &format!("oci:{}:base", layout.display()), verbose)
            .context("read OCI image environment")?;

    let mut unpack = mapped_command(&tools.unshare, &tools.umoci);
    unpack
        .arg("unpack")
        .arg("--image")
        .arg(format!("{}:base", layout.display()))
        .arg(&bundle);
    run_checked(&mut unpack, verbose, "unpack OCI filesystem layers")?;
    let rootfs = bundle.join("rootfs");
    provision_rootfs(tools, &rootfs)
        .context("inject Spawnr guest integration into unpacked OCI rootfs")?;

    let disk = temporary.join(CACHE_DISK);
    let required = directory_apparent_size(tools, &rootfs, verbose)
        .context("measure unpacked OCI root filesystem")?
        .saturating_mul(2)
        .saturating_add(4 * 1024 * 1024 * 1024);
    let disk_bytes = DEFAULT_ENVIRONMENT_BYTES.max(required);
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&disk)
        .context("create sparse OCI environment filesystem")?;
    file.set_len(disk_bytes)
        .context("size sparse OCI environment filesystem")?;
    drop(file);

    let mut mkfs = mapped_command(&tools.unshare, &tools.mkfs_ext4);
    mkfs.args(["-q", "-F", "-m", "0", "-L", "SPAWNR_ENV", "-d"])
        .arg(&rootfs)
        .arg(&disk);
    run_checked(&mut mkfs, verbose, "materialize OCI environment as ext4")?;
    validate_ext4_image(&disk).context("validate materialized OCI ext4 filesystem")?;

    let metadata = PreparedMetadata {
        source_reference: user_reference.to_owned(),
        source_digest: digest.to_owned(),
        cache_key: cache_key.to_owned(),
        architecture: "amd64".into(),
        os: "linux".into(),
        config_env: Some(image_environment),
    };
    write_json(&temporary.join(CACHE_METADATA), &metadata)
        .context("write immutable OCI cache metadata")?;

    // The mtree bundle is useful only while preparing the base; publishing
    // recreates it from the retained content-addressed OCI layout.
    remove_tree_mapped(tools, &bundle).context("remove transient OCI unpack bundle")?;
    sync_directory(temporary).context("sync completed OCI cache entry")
}

fn provision_rootfs(tools: &Tools, rootfs: &Path) -> Result<()> {
    // Treat the unpacked image as hostile input. In particular, never let an
    // image-controlled symlink redirect integration writes outside rootfs.
    let libexec = ensure_safe_directory(rootfs, Path::new("usr/libexec"))?;
    copy_regular(&tools.agent, &libexec.join("spawnr-agent"), 0o4755)
        .context("install static spawnr-agent")?;
    copy_regular(&tools.busybox, &libexec.join("spawnr-busybox"), 0o755)
        .context("install static BusyBox")?;
    let dhcp = libexec.join("spawnr-udhcpc");
    write_executable(&dhcp, include_bytes!("../../../guest/assets/spawnr-udhcpc"))
        .context("install guest DHCP hook")?;
    ensure_safe_directory(rootfs, Path::new("workspace"))
        .context("create guest workspace mount point")?;
    ensure_safe_directory(rootfs, Path::new("run/spawnr"))
        .context("create guest session mount point")?;
    let etc = ensure_safe_directory(rootfs, Path::new("etc"))?;
    let resolv = etc.join("resolv.conf");
    match fs::symlink_metadata(&resolv) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            fs::remove_file(&resolv)?
        }
        Ok(_) => bail!("guest /etc/resolv.conf is not a regular file or symlink"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect guest resolv.conf"),
    }
    std::os::unix::fs::symlink("/run/spawnr/resolv.conf", &resolv)
        .context("make guest DNS state session-ephemeral")?;
    Ok(())
}

impl Tools {
    fn discover(paths: &Paths) -> Result<Self> {
        Ok(Self {
            skopeo: resolve_tool(paths, "SPAWNR_SKOPEO", "skopeo", "skopeo")?,
            ca_certificates: if env::var_os("SSL_CERT_FILE").is_some() {
                None
            } else {
                crate::runtime_install::component_path(paths, "ca-certificates")?
            },
            umoci: resolve_tool(paths, "SPAWNR_UMOCI", "umoci", "umoci")?,
            unshare: resolve_tool(paths, "SPAWNR_UNSHARE", "unshare", "unshare")?,
            mkfs_ext4: resolve_tool(paths, "SPAWNR_MKFS_EXT4", "mkfs.ext4", "mkfs-ext4")?,
            du: resolve_tool(paths, "SPAWNR_DU", "du", "du")?,
            agent: resolve_asset(paths, "SPAWNR_AGENT", "spawnr-agent", "spawnr-agent")?,
            busybox: resolve_asset(paths, "SPAWNR_BUSYBOX", "spawnr-busybox", "busybox")?,
        })
    }
}

impl PublishTools {
    fn discover(paths: &Paths) -> Result<Self> {
        Ok(Self {
            common: Tools::discover(paths)?,
            e2fsck: resolve_tool(paths, "SPAWNR_E2FSCK", "e2fsck", "e2fsck")?,
            fuse2fs: resolve_tool(paths, "SPAWNR_FUSE2FS", "fuse2fs", "fuse2fs")?,
            fusermount: resolve_one_of(paths, "SPAWNR_FUSERMOUNT", &["fusermount3", "fusermount"])?,
        })
    }
}

fn resolve_tool(paths: &Paths, variable: &str, name: &str, component: &str) -> Result<PathBuf> {
    let bundled = crate::runtime_install::preferred_component(
        paths,
        variable,
        component,
        &paths.bin_dir().join(name),
    )?;
    process::resolve_executable(variable, &bundled, name)
        .with_context(|| format!("{name} is required for direct OCI environments"))
}

fn skopeo_command(tools: &Tools) -> Command {
    let mut command = Command::new(&tools.skopeo);
    if let Some(certificates) = &tools.ca_certificates {
        command.env("SSL_CERT_FILE", certificates);
    }
    command
}

fn resolve_one_of(paths: &Paths, variable: &str, names: &[&str]) -> Result<PathBuf> {
    if let Some(value) = env::var_os(variable) {
        return process::validate_executable(Path::new(&value));
    }
    if let Some(path) = crate::runtime_install::component_path(paths, "fusermount3")? {
        return process::validate_executable(&path);
    }
    for name in names {
        if let Ok(path) = process::resolve_executable(variable, &paths.bin_dir().join(name), name) {
            return Ok(path);
        }
    }
    bail!("cannot find {}; set {variable}", names.join(" or "))
}

fn resolve_asset(paths: &Paths, variable: &str, name: &str, component: &str) -> Result<PathBuf> {
    let selected = if let Some(value) = env::var_os(variable) {
        process::validate_executable(Path::new(&value))?
    } else {
        let bundled = paths.bin_dir().join(name);
        let preferred =
            crate::runtime_install::preferred_component(paths, variable, component, &bundled)?;
        if preferred != bundled || bundled.exists() {
            process::validate_executable(&preferred)?
        } else {
            // Development builds place both host and guest binaries together.
            let sibling = env::current_exe()
                .ok()
                .and_then(|current| current.parent().map(|directory| directory.join(name)))
                .filter(|sibling| sibling.exists());
            let Some(sibling) = sibling else {
                bail!(
                    "cannot find guest asset {name}; install it at {} or set {variable}",
                    bundled.display()
                );
            };
            process::validate_executable(&sibling)?
        }
    };
    validate_static_x86_64_elf(&selected)
        .with_context(|| format!("guest asset {name} must be a static x86_64 ELF binary"))?;
    Ok(selected)
}

fn validate_static_x86_64_elf(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)?;
    ensure!(&header[..4] == b"\x7fELF", "missing ELF header");
    ensure!(
        header[4] == 2 && header[5] == 1,
        "ELF is not 64-bit little-endian"
    );
    ensure!(
        u16::from_le_bytes(header[18..20].try_into()?) == 62,
        "ELF is not x86_64"
    );
    let program_offset = u64::from_le_bytes(header[32..40].try_into()?);
    let entry_size = u16::from_le_bytes(header[54..56].try_into()?) as u64;
    let entry_count = u16::from_le_bytes(header[56..58].try_into()?) as u64;
    ensure!(
        entry_size >= 56 && entry_count <= 256,
        "invalid ELF program headers"
    );
    ensure!(
        program_offset.saturating_add(entry_size.saturating_mul(entry_count)) <= length,
        "ELF program headers exceed file length"
    );
    let mut found_load = false;
    for index in 0..entry_count {
        file.seek(SeekFrom::Start(program_offset + index * entry_size))?;
        let mut kind = [0_u8; 4];
        file.read_exact(&mut kind)?;
        match u32::from_le_bytes(kind) {
            1 => found_load = true,
            3 => bail!("ELF has a dynamic interpreter (PT_INTERP)"),
            _ => {}
        }
    }
    ensure!(found_load, "ELF has no loadable segment");
    Ok(())
}

fn inspect_digest(tools: &Tools, reference: &str, verbose: u8) -> Result<String> {
    let mut command = skopeo_command(tools);
    command
        .arg("--insecure-policy")
        .arg("inspect")
        .args(["--override-os", "linux", "--override-arch", "amd64"])
        .args(["--format", "{{.Digest}}"])
        .arg(reference);
    let output = run_output(&mut command, verbose, "resolve OCI environment digest")?;
    let digest = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure_manifest_digest(&digest).context("registry returned an invalid manifest digest")?;
    Ok(digest)
}

/// Read Config.Env from the immutable layout produced by `skopeo copy`.
///
/// Using the copied layout is important: consulting the user's source tag a
/// second time would let a concurrent tag move pair one manifest's digest with
/// another manifest's environment. The Go-template JSON encoder gives us only
/// the Env array rather than the rest of the potentially large image config.
fn inspect_image_environment(
    tools: &Tools,
    copied_reference: &str,
    verbose: u8,
) -> Result<Vec<String>> {
    ensure!(
        copied_reference.starts_with("oci:"),
        "image environment must be inspected from a copied OCI layout"
    );
    let mut command = skopeo_command(tools);
    command
        .arg("--insecure-policy")
        .arg("inspect")
        .args(["--override-os", "linux", "--override-arch", "amd64"])
        .args(["--format", "{{json .Env}}"])
        .arg(copied_reference);
    let output = run_output_bounded(
        &mut command,
        verbose,
        "read OCI image environment",
        MAX_GUEST_ENVIRONMENT_JSON_BYTES,
    )?;
    parse_image_environment_json(&output.stdout)
}

fn parse_image_environment_json(encoded: &[u8]) -> Result<Vec<String>> {
    ensure!(
        encoded.len() <= MAX_GUEST_ENVIRONMENT_JSON_BYTES,
        "OCI Config.Env JSON exceeds {MAX_GUEST_ENVIRONMENT_JSON_BYTES} bytes"
    );
    // Skopeo renders a missing Env as JSON null. Treat that identically to an
    // empty array, while rejecting every other unexpected template shape.
    let environment: Option<Vec<String>> =
        serde_json::from_slice(encoded).context("decode OCI Config.Env JSON")?;
    let environment = environment.unwrap_or_default();
    validate_image_environment(&environment)?;
    Ok(environment)
}

fn validate_image_environment(environment: &[String]) -> Result<()> {
    ensure!(
        environment.len() <= MAX_GUEST_ENVIRONMENT_ITEMS,
        "OCI Config.Env has more than {MAX_GUEST_ENVIRONMENT_ITEMS} entries"
    );
    let mut total_bytes = 0_usize;
    for (index, entry) in environment.iter().enumerate() {
        total_bytes = total_bytes
            .checked_add(entry.len())
            .and_then(|bytes| bytes.checked_add(1))
            .context("OCI Config.Env size overflow")?;
        ensure!(
            total_bytes <= MAX_GUEST_ENVIRONMENT_BYTES,
            "OCI Config.Env exceeds {MAX_GUEST_ENVIRONMENT_BYTES} bytes"
        );
        let (name, value) = entry
            .split_once('=')
            .with_context(|| format!("OCI Config.Env entry {index} has no '=' separator"))?;
        // OCI follows execve(2), not shell assignment syntax. Names such as
        // `1NAME` and `org.example.option` are valid process-environment keys
        // even though a shell cannot create them with `export`.
        ensure!(
            !name.is_empty() && !name.contains('\0'),
            "OCI Config.Env entry {index} has an invalid variable name"
        );
        ensure!(
            !value.contains('\0'),
            "OCI Config.Env entry {index} contains NUL"
        );
    }
    Ok(())
}

fn validate_prepared_metadata(
    metadata: &PreparedMetadata,
    require_environment: bool,
) -> Result<()> {
    normalize_source_reference(&metadata.source_reference)
        .context("cache metadata has an invalid source reference")?;
    ensure_manifest_digest(&metadata.source_digest)
        .context("cache metadata has an invalid source digest")?;
    ensure_hex_digest(&metadata.cache_key).context("cache metadata has an invalid cache key")?;
    ensure!(
        metadata.architecture == "amd64" && metadata.os == "linux",
        "cache metadata has unsupported platform {}/{}",
        metadata.os,
        metadata.architecture
    );
    match &metadata.config_env {
        Some(environment) => validate_image_environment(environment),
        None if require_environment => {
            bail!("cache metadata does not contain the OCI Config.Env field")
        }
        None => Ok(()),
    }
}

fn read_prepared_metadata(path: &Path) -> Result<PreparedMetadata> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let file_metadata = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        file_metadata.is_file(),
        "OCI cache metadata {} is not a regular file",
        path.display()
    );
    ensure!(
        file_metadata.len() <= MAX_GUEST_ENVIRONMENT_JSON_BYTES as u64,
        "OCI cache metadata exceeds {MAX_GUEST_ENVIRONMENT_JSON_BYTES} bytes"
    );
    let mut encoded = Vec::with_capacity(file_metadata.len() as usize);
    (&mut file)
        .take(MAX_GUEST_ENVIRONMENT_JSON_BYTES as u64 + 1)
        .read_to_end(&mut encoded)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        encoded.len() <= MAX_GUEST_ENVIRONMENT_JSON_BYTES,
        "OCI cache metadata exceeds {MAX_GUEST_ENVIRONMENT_JSON_BYTES} bytes"
    );
    serde_json::from_slice(&encoded).context("decode OCI cache metadata")
}

fn integration_cache_key(tools: &Tools, source_digest: &str) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(INTEGRATION_CACHE_SCHEMA);
    digest.update([0]);
    digest.update(env!("CARGO_PKG_VERSION").as_bytes());
    digest.update([0]);
    digest.update(source_digest.as_bytes());
    digest.update([0]);
    hash_file(&mut digest, &tools.agent)?;
    digest.update([0]);
    hash_file(&mut digest, &tools.busybox)?;
    digest.update([0]);
    digest.update(include_bytes!("../../../guest/assets/spawnr-udhcpc"));
    Ok(format!("{:x}", digest.finalize()))
}

fn hash_file(digest: &mut Sha256, path: &Path) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("open cache input {}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("hash cache input {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(())
}

fn normalize_source_reference(reference: &str) -> Result<String> {
    let reference = match reference {
        "ubuntu" => "docker.io/library/ubuntu:24.04",
        other => other,
    };
    normalize_reference(reference)
}

fn normalize_destination_reference(reference: &str) -> Result<String> {
    normalize_reference(reference)
}

fn immutable_pull_reference(reference: &str, digest: &str) -> Result<String> {
    ensure!(
        digest
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "invalid OCI manifest digest {digest:?}"
    );
    let Some(name) = reference.strip_prefix("docker://") else {
        // Local OCI layout tags cannot be expressed as digest references by
        // the transport. build_cache verifies their copied descriptor before
        // it commits the cache instead.
        return Ok(reference.to_owned());
    };
    let mut name = name.split('@').next().unwrap_or(name).to_owned();
    let last_slash = name.rfind('/');
    if let Some(last_colon) = name.rfind(':')
        && last_slash.is_none_or(|slash| last_colon > slash)
    {
        name.truncate(last_colon);
    }
    ensure!(
        !name.is_empty(),
        "invalid registry OCI reference {reference:?}"
    );
    Ok(format!("docker://{name}@{digest}"))
}

fn normalize_reference(reference: &str) -> Result<String> {
    ensure!(
        !reference.is_empty()
            && !reference.starts_with('-')
            && !reference.contains(['\n', '\r', '\0'])
            && !reference.chars().any(char::is_whitespace),
        "invalid OCI reference {reference:?}"
    );
    const FORBIDDEN_LOCAL_TRANSPORTS: [&str; 5] = [
        "docker-daemon:",
        "containers-storage:",
        "dir:",
        "tarball:",
        "ostree:",
    ];
    ensure!(
        !FORBIDDEN_LOCAL_TRANSPORTS
            .iter()
            .any(|transport| reference.starts_with(transport)),
        "unsupported OCI transport in {reference:?}; Spawnr never uses a container daemon"
    );
    if reference.starts_with("docker://") || reference.starts_with("oci:") {
        Ok(reference.to_owned())
    } else {
        ensure!(
            !reference.contains("://"),
            "unsupported OCI transport in {reference:?}"
        );
        Ok(format!("docker://{reference}"))
    }
}

fn valid_cache(cache: &Path, disk: &Path, digest: &str, cache_key: &str) -> Result<bool> {
    if !cache.exists() {
        return Ok(false);
    }
    if validate_ext4_image(disk).is_err() {
        return Ok(false);
    }
    let metadata = match read_prepared_metadata(&cache.join(CACHE_METADATA)) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    if validate_prepared_metadata(&metadata, true).is_err() {
        return Ok(false);
    }
    Ok(metadata.source_digest == digest
        && metadata.cache_key == cache_key
        && metadata.architecture == "amd64"
        && metadata.os == "linux")
}

fn mapped_command(unshare: &Path, program: &Path) -> Command {
    let mut command = Command::new(unshare);
    command
        .args([
            "--user",
            "--map-auto",
            "--map-root-user",
            "--mount",
            "--fork",
            "--",
        ])
        .arg(program);
    command
}

fn run_checked(command: &mut Command, verbose: u8, operation: &str) -> Result<()> {
    let output = run_output(command, verbose, operation)?;
    ensure!(output.status.success(), "{operation} failed");
    Ok(())
}

fn run_output(command: &mut Command, verbose: u8, operation: &str) -> Result<Output> {
    command.stdin(Stdio::null());
    let output = command
        .output()
        .with_context(|| format!("{operation}: execute {:?}", command.get_program()))?;
    if verbose > 0 && !output.stderr.is_empty() {
        std::io::stderr().write_all(&output.stderr).ok();
    }
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        bail!("{operation} ({})\n{}", output.status, diagnostic.trim_end());
    }
    Ok(output)
}

/// Execute a command while retaining at most `stdout_limit + 1` bytes from
/// stdout. Stderr is drained concurrently to avoid a full pipe deadlock, but
/// only a bounded diagnostic prefix is retained.
fn run_output_bounded(
    command: &mut Command,
    verbose: u8,
    operation: &str,
    stdout_limit: usize,
) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("{operation}: execute {:?}", command.get_program()))?;
    let mut stdout = child.stdout.take().context("capture subprocess stdout")?;
    let stderr = child.stderr.take().context("capture subprocess stderr")?;
    let stderr_thread =
        thread::spawn(move || drain_bounded(stderr, MAX_SUBPROCESS_DIAGNOSTIC_BYTES));

    let mut stdout_bytes = Vec::with_capacity(stdout_limit.min(64 * 1024));
    let stdout_result = (&mut stdout)
        .take(stdout_limit as u64 + 1)
        .read_to_end(&mut stdout_bytes);
    if let Err(error) = stdout_result {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_thread.join();
        return Err(error).with_context(|| format!("{operation}: read stdout"));
    }
    if stdout_bytes.len() > stdout_limit {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_thread.join();
        bail!("{operation}: stdout exceeds {stdout_limit} bytes");
    }

    let status = child
        .wait()
        .with_context(|| format!("{operation}: wait for subprocess"))?;
    let stderr_bytes = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{operation}: stderr reader panicked"))?
        .with_context(|| format!("{operation}: read stderr"))?;
    if verbose > 0 && !stderr_bytes.is_empty() {
        std::io::stderr().write_all(&stderr_bytes).ok();
    }
    if !status.success() {
        let diagnostic = String::from_utf8_lossy(&stderr_bytes);
        bail!("{operation} ({status})\n{}", diagnostic.trim_end());
    }
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

fn drain_bounded(mut reader: impl Read, retained_limit: usize) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(retained_limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(retained);
        }
        let remaining = retained_limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..count.min(remaining)]);
    }
}

fn run_e2fsck(program: &Path, disk: &Path, verbose: u8) -> Result<()> {
    let output = Command::new(program)
        .args(["-p", "-f"])
        .arg(disk)
        .output()?;
    if verbose > 0 {
        std::io::stderr().write_all(&output.stderr).ok();
    }
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "e2fsck failed ({})",
        output.status
    );
    Ok(())
}

fn remove_tree_mapped(tools: &Tools, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let rm = process::find_executable("rm").context("rm is required for mapped OCI cleanup")?;
    let mut command = mapped_command(&tools.unshare, &rm);
    command.args(["-rf", "--"]).arg(path);
    let status = command.status()?;
    ensure!(status.success(), "failed to clean temporary OCI directory");
    Ok(())
}

fn directory_apparent_size(tools: &Tools, root: &Path, verbose: u8) -> Result<u64> {
    // OCI ownership is materialized through subordinate IDs. Some images
    // contain mode-0700 directories owned by a non-root image user, so the
    // ordinary host process cannot traverse them. Re-enter the same mapping;
    // namespace root then has the exact privilege needed to measure the tree.
    let mut command = mapped_command(&tools.unshare, &tools.du);
    command
        .args(["--bytes", "--summarize", "--apparent-size", "--"])
        .arg(root);
    let output = run_output(&mut command, verbose, "measure OCI root filesystem")?;
    let output = std::str::from_utf8(&output.stdout).context("du returned non-UTF-8 output")?;
    output
        .split_ascii_whitespace()
        .next()
        .context("du returned no filesystem size")?
        .parse::<u64>()
        .context("du returned an invalid filesystem size")
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            copy_tree(&source, &destination)?;
        } else if metadata.is_file() {
            fs::copy(&source, &destination)?;
        } else if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&source)?, &destination)?;
        } else {
            bail!("unexpected file type in OCI layout: {}", source.display());
        }
    }
    Ok(())
}

fn ensure_safe_directory(root: &Path, relative: &Path) -> Result<PathBuf> {
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect OCI root {}", root.display()))?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "OCI root is not a real directory: {}",
        root.display()
    );

    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            bail!(
                "integration directory must be a relative normal path: {}",
                relative.display()
            );
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "refusing OCI-controlled non-directory path {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!("create integration directory {}", current.display())
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect integration directory {}", current.display())
                });
            }
        }
    }
    Ok(current)
}

fn copy_regular(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    ensure!(
        fs::symlink_metadata(source)?.is_file(),
        "{} is not a file",
        source.display()
    );
    remove_replaceable_file(destination)?;

    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(source)
        .with_context(|| format!("open integration source {}", source.display()))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(destination)
        .with_context(|| format!("create integration asset {}", destination.display()))?;
    std::io::copy(&mut input, &mut output)
        .with_context(|| format!("copy integration asset to {}", destination.display()))?;
    output.sync_all()?;
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn write_executable(path: &Path, contents: &[u8]) -> Result<()> {
    remove_replaceable_file(path)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o755)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn remove_replaceable_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
        }
        Ok(_) => bail!(
            "refusing to replace non-file integration asset {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn ensure_hex_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid sha256 digest {value:?}"
    );
    Ok(())
}

fn ensure_manifest_digest(value: &str) -> Result<()> {
    let hexadecimal = value
        .strip_prefix("sha256:")
        .context("OCI manifest digest does not use sha256")?;
    ensure_hex_digest(hexadecimal).context("invalid OCI manifest digest")
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

struct CacheLock(File);

impl CacheLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        // SAFETY: flock operates only on this live file descriptor.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "lock OCI cache: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(file))
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // SAFETY: unlock the still-live descriptor before File drops.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

const REPACK_SCRIPT: &str = r#"#!/bin/sh
set -eu
umoci=$1
fuse2fs=$2
fusermount=$3
layout=$4
disk=$5
bundle=$6
"$umoci" unpack --image "$layout:base" "$bundle"
"$fuse2fs" -o ro "$disk" "$bundle/rootfs"
mounted=1
cleanup() {
  if test "${mounted:-0}" = 1; then
    "$fusermount" -u "$bundle/rootfs" || true
  fi
}
trap cleanup EXIT INT TERM
"$umoci" repack \
  --no-mask-volumes \
  --image "$layout:published" \
  --history.created_by "spawnr publish" \
  --history.comment "Spawnr environment filesystem" \
  --compress gzip \
  "$bundle"
"$fusermount" -u "$bundle/rootfs"
mounted=0
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_with_environment(config_env: Option<Vec<String>>) -> PreparedMetadata {
        PreparedMetadata {
            source_reference: "ubuntu".into(),
            source_digest: format!("sha256:{}", "1".repeat(64)),
            cache_key: "2".repeat(64),
            architecture: "amd64".into(),
            os: "linux".into(),
            config_env,
        }
    }

    #[test]
    fn normalizes_registry_references_without_docker_daemon() {
        assert_eq!(
            normalize_source_reference("ghcr.io/acme/dev:v1").unwrap(),
            "docker://ghcr.io/acme/dev:v1"
        );
        assert_eq!(
            normalize_source_reference("ubuntu").unwrap(),
            "docker://docker.io/library/ubuntu:24.04"
        );
        assert!(normalize_source_reference("docker-daemon:ubuntu").is_err());
    }

    #[test]
    fn validates_content_addressed_cache_keys() {
        assert!(ensure_hex_digest(&"a".repeat(64)).is_ok());
        assert!(ensure_hex_digest("../../workspace").is_err());
    }

    #[test]
    fn publish_script_has_structural_single_disk_input() {
        assert!(REPACK_SCRIPT.contains("$disk"));
        assert!(REPACK_SCRIPT.contains("--no-mask-volumes"));
        assert!(!REPACK_SCRIPT.contains("workspace"));
        assert!(!REPACK_SCRIPT.contains("session"));
    }

    #[test]
    fn pins_mutable_registry_tags_to_the_resolved_digest() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            immutable_pull_reference("docker://registry.example:5000/acme/dev:v4", &digest)
                .unwrap(),
            format!("docker://registry.example:5000/acme/dev@{digest}")
        );
        assert_eq!(
            immutable_pull_reference("oci:/tmp/layout:v4", &digest).unwrap(),
            "oci:/tmp/layout:v4"
        );
    }

    #[test]
    fn parses_ordered_oci_environment_json() {
        let environment = parse_image_environment_json(
            br#"["PATH=/opt/bin:/usr/bin","EMPTY=","TOKEN=a=b","PATH=/bin"]"#,
        )
        .unwrap();
        assert_eq!(
            environment,
            ["PATH=/opt/bin:/usr/bin", "EMPTY=", "TOKEN=a=b", "PATH=/bin"]
        );
        assert!(parse_image_environment_json(b"null").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_or_oversized_oci_environment() {
        for encoded in [
            br#"["MISSING_SEPARATOR"]"#.as_slice(),
            br#"["=empty-name"]"#.as_slice(),
            br#"["BAD\u0000NAME=value"]"#.as_slice(),
            br#"["GOOD=contains\u0000nul"]"#.as_slice(),
        ] {
            assert!(parse_image_environment_json(encoded).is_err());
        }

        assert!(
            parse_image_environment_json(
                br#"["9STARTS_WITH_DIGIT=value","HAS-DASH=value","org.example.option=yes"]"#
            )
            .is_ok()
        );

        let too_many = vec!["A=value".to_owned(); MAX_GUEST_ENVIRONMENT_ITEMS + 1];
        assert!(validate_image_environment(&too_many).is_err());
        let too_large = vec![format!("A={}", "x".repeat(MAX_GUEST_ENVIRONMENT_BYTES))];
        assert!(validate_image_environment(&too_large).is_err());
        assert!(
            parse_image_environment_json(&vec![b' '; MAX_GUEST_ENVIRONMENT_JSON_BYTES + 1])
                .is_err()
        );
    }

    #[test]
    fn v3_cache_metadata_requires_a_valid_environment_field() {
        let valid = metadata_with_environment(Some(vec!["PATH=/usr/bin:/bin".into()]));
        validate_prepared_metadata(&valid, true).unwrap();

        let legacy = metadata_with_environment(None);
        assert!(validate_prepared_metadata(&legacy, false).is_ok());
        assert!(validate_prepared_metadata(&legacy, true).is_err());

        let invalid = metadata_with_environment(Some(vec!["=empty-name".into()]));
        assert!(validate_prepared_metadata(&invalid, true).is_err());
    }

    #[test]
    fn cache_validation_rejects_missing_environment_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let disk = cache.join(CACHE_DISK);
        let mut file = File::create(&disk).unwrap();
        file.set_len(2048).unwrap();
        file.seek(SeekFrom::Start(1024 + 0x38)).unwrap();
        file.write_all(&0xEF53_u16.to_le_bytes()).unwrap();
        drop(file);

        let valid = metadata_with_environment(Some(Vec::new()));
        write_json(&cache.join(CACHE_METADATA), &valid).unwrap();
        assert!(valid_cache(&cache, &disk, &valid.source_digest, &valid.cache_key).unwrap());

        fs::remove_file(cache.join(CACHE_METADATA)).unwrap();
        let legacy = metadata_with_environment(None);
        write_json(&cache.join(CACHE_METADATA), &legacy).unwrap();
        assert!(!valid_cache(&cache, &disk, &legacy.source_digest, &legacy.cache_key).unwrap());
    }

    #[test]
    fn rejects_guest_elf_with_dynamic_interpreter() {
        let path = env::temp_dir().join(format!("spawnr-elf-{}", Uuid::new_v4()));
        let mut elf = vec![0_u8; 120];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
        elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1_u16.to_le_bytes());
        elf[64..68].copy_from_slice(&1_u32.to_le_bytes());
        fs::write(&path, &elf).unwrap();
        validate_static_x86_64_elf(&path).unwrap();
        elf[64..68].copy_from_slice(&3_u32.to_le_bytes());
        fs::write(&path, &elf).unwrap();
        assert!(validate_static_x86_64_elf(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_image_symlink_that_redirects_integration_writes() {
        let nonce = Uuid::new_v4();
        let root = env::temp_dir().join(format!("spawnr-rootfs-{nonce}"));
        let outside = env::temp_dir().join(format!("spawnr-outside-{nonce}"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("usr")).unwrap();

        let error = ensure_safe_directory(&root, Path::new("usr/libexec")).unwrap_err();
        assert!(error.to_string().contains("non-directory"));
        assert!(!outside.join("libexec").exists());

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }
}
