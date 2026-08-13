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
use uuid::Uuid;

const DEFAULT_ENVIRONMENT_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const CACHE_METADATA: &str = "spawnr-environment.json";
const CACHE_DISK: &str = "environment.raw";
const INTEGRATION_CACHE_SCHEMA: &[u8] = b"spawnr-guest-integration-v2";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedMetadata {
    pub source_reference: String,
    pub source_digest: String,
    pub cache_key: String,
    pub architecture: String,
    pub os: String,
}

#[derive(Debug, Clone)]
struct Tools {
    skopeo: PathBuf,
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
    let file =
        File::open(directory.join(CACHE_METADATA)).context("open prepared environment metadata")?;
    serde_json::from_reader(file).context("decode prepared environment metadata")
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
    let metadata: PreparedMetadata = serde_json::from_reader(
        File::open(cache.join(CACHE_METADATA)).context("open OCI base metadata")?,
    )?;
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

        let mut command = Command::new(&tools.common.skopeo);
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
    let mut pull = Command::new(&tools.skopeo);
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
            skopeo: resolve_tool(paths, "SPAWNR_SKOPEO", "skopeo")?,
            umoci: resolve_tool(paths, "SPAWNR_UMOCI", "umoci")?,
            unshare: resolve_tool(paths, "SPAWNR_UNSHARE", "unshare")?,
            mkfs_ext4: resolve_tool(paths, "SPAWNR_MKFS_EXT4", "mkfs.ext4")?,
            du: resolve_tool(paths, "SPAWNR_DU", "du")?,
            agent: resolve_asset(paths, "SPAWNR_AGENT", "spawnr-agent")?,
            busybox: resolve_asset(paths, "SPAWNR_BUSYBOX", "spawnr-busybox")?,
        })
    }
}

impl PublishTools {
    fn discover(paths: &Paths) -> Result<Self> {
        Ok(Self {
            common: Tools::discover(paths)?,
            e2fsck: resolve_tool(paths, "SPAWNR_E2FSCK", "e2fsck")?,
            fuse2fs: resolve_tool(paths, "SPAWNR_FUSE2FS", "fuse2fs")?,
            fusermount: resolve_one_of(paths, "SPAWNR_FUSERMOUNT", &["fusermount3", "fusermount"])?,
        })
    }
}

fn resolve_tool(paths: &Paths, variable: &str, name: &str) -> Result<PathBuf> {
    process::resolve_executable(variable, &paths.bin_dir().join(name), name)
        .with_context(|| format!("{name} is required for direct OCI environments"))
}

fn resolve_one_of(paths: &Paths, variable: &str, names: &[&str]) -> Result<PathBuf> {
    if let Some(value) = env::var_os(variable) {
        return process::validate_executable(Path::new(&value));
    }
    for name in names {
        if let Ok(path) = process::resolve_executable(variable, &paths.bin_dir().join(name), name) {
            return Ok(path);
        }
    }
    bail!("cannot find {}; set {variable}", names.join(" or "))
}

fn resolve_asset(paths: &Paths, variable: &str, name: &str) -> Result<PathBuf> {
    let selected = if let Some(value) = env::var_os(variable) {
        process::validate_executable(Path::new(&value))?
    } else {
        let bundled = paths.bin_dir().join(name);
        if bundled.exists() {
            process::validate_executable(&bundled)?
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
    let mut command = Command::new(&tools.skopeo);
    command
        .arg("--insecure-policy")
        .arg("inspect")
        .args(["--override-os", "linux", "--override-arch", "amd64"])
        .args(["--format", "{{.Digest}}"])
        .arg(reference);
    let output = run_output(&mut command, verbose, "resolve OCI environment digest")?;
    let digest = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(
        digest.starts_with("sha256:"),
        "registry returned invalid digest {digest:?}"
    );
    Ok(digest)
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
    let file = match File::open(cache.join(CACHE_METADATA)) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    let metadata: PreparedMetadata = match serde_json::from_reader(file) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    Ok(metadata.source_digest == digest && metadata.cache_key == cache_key)
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
