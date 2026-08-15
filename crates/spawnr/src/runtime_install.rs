//! Download, verification, atomic installation, and discovery of the managed runtime.

use crate::paths::{Paths, create_private_dir};
use crate::runtime::{RuntimeLock, RuntimeManifest, validate_runtime_path};
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const ACTIVE_SCHEMA_VERSION: u32 = 1;
const MAX_LOCK_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;
const DOWNLOAD_TIMEOUT_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone)]
pub struct InstalledRuntime {
    root: PathBuf,
    manifest: RuntimeManifest,
    manifest_sha256: String,
}

#[derive(Debug, Clone)]
pub struct SetupOutcome {
    pub installation: InstalledRuntime,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ActiveRuntime {
    schema_version: u32,
    runtime_version: String,
    manifest_sha256: String,
}

#[derive(Debug)]
struct ActualFile {
    size_bytes: u64,
    sha256: String,
    executable: bool,
}

struct SetupLock {
    _file: File,
}

impl InstalledRuntime {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version(&self) -> &str {
        &self.manifest.runtime_version
    }

    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    pub fn component(&self, name: &str) -> Result<PathBuf> {
        let component = self
            .manifest
            .components
            .iter()
            .find(|component| component.name == name)
            .with_context(|| format!("managed runtime has no component {name}"))?;
        let expected = self
            .manifest
            .files
            .iter()
            .find(|file| file.path == component.path)
            .expect("validated component references a runtime file");
        let path = self.root.join(&component.path);
        let canonical_root = fs::canonicalize(&self.root).context("canonicalize runtime root")?;
        let canonical_path = fs::canonicalize(&path)
            .with_context(|| format!("canonicalize managed runtime component {name}"))?;
        ensure!(
            canonical_path.starts_with(&canonical_root),
            "managed runtime component escapes its versioned root"
        );
        verify_runtime_file(&path, expected)
            .with_context(|| format!("verify managed runtime component {name}"))?;
        Ok(path)
    }
}

pub fn setup(
    paths: &Paths,
    lock_path: Option<&Path>,
    archive_path: Option<&Path>,
) -> Result<SetupOutcome> {
    create_private_dir(paths.root()).context("create Spawnr data directory")?;
    create_private_dir(&paths.runtime_dir()).context("create managed runtime directory")?;
    let _setup_lock = SetupLock::acquire(&paths.runtime_lock_file())?;
    let lock = load_runtime_lock(lock_path)?;
    let cli_version = Version::parse(env!("CARGO_PKG_VERSION")).expect("valid package version");
    lock.validate_for_cli(&cli_version)?;

    let destination = paths.runtime_version_dir(&lock.runtime_version);
    if destination.exists()
        && let Ok(installation) = verify_installation_at(&destination, Some(&lock))
    {
        write_active(paths, &installation)?;
        return Ok(SetupOutcome {
            installation,
            installed: false,
        });
    }

    let downloaded = if archive_path.is_none() {
        Some(download_archive(paths, &lock)?)
    } else {
        None
    };
    let archive = archive_path
        .or(downloaded.as_deref())
        .expect("archive path or downloaded archive");
    verify_archive(archive, &lock)?;

    let staging = paths.runtime_dir().join(format!(
        ".install-{}-{}",
        lock.runtime_version,
        Uuid::new_v4()
    ));
    create_private_dir(&staging).context("create temporary runtime installation")?;
    let result = (|| -> Result<InstalledRuntime> {
        extract_archive(archive, &staging, &lock)?;
        let installation = verify_installation_at(&staging, Some(&lock))?;
        install_staging(paths, &staging, &destination, &installation)?;
        verify_installation_at(&destination, Some(&lock))
    })();
    if result.is_err() {
        remove_any(&staging).ok();
    }
    if archive_path.is_none() {
        fs::remove_file(archive).ok();
    }
    result.map(|installation| SetupOutcome {
        installation,
        installed: true,
    })
}

pub fn discover(paths: &Paths) -> Result<Option<InstalledRuntime>> {
    let active_path = paths.runtime_active_file();
    let bytes = match read_limited(&active_path, MAX_LOCK_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read active managed runtime"),
    };
    let active: ActiveRuntime =
        serde_json::from_slice(&bytes).context("decode active managed runtime")?;
    ensure!(
        active.schema_version == ACTIVE_SCHEMA_VERSION,
        "unsupported active runtime schema {}",
        active.schema_version
    );
    let active_version = Version::parse(&active.runtime_version)
        .context("active runtime contains an invalid version")?;
    ensure!(
        active_version.build.is_empty() && active_version.to_string() == active.runtime_version,
        "active runtime version is not canonical"
    );
    ensure!(
        active.manifest_sha256.len() == 64
            && active
                .manifest_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "active runtime manifest digest is invalid"
    );
    let root = paths.runtime_version_dir(&active.runtime_version);
    let installation = load_installation(&root)?;
    ensure!(
        installation.version() == active.runtime_version,
        "active runtime version does not match its manifest"
    );
    ensure!(
        installation.manifest_sha256 == active.manifest_sha256,
        "active runtime manifest digest does not match active.json"
    );
    let cli_version = Version::parse(env!("CARGO_PKG_VERSION")).expect("valid package version");
    installation
        .manifest
        .cli_compatibility
        .ensure_contains(&cli_version)?;
    Ok(Some(installation))
}

pub fn verify_active(paths: &Paths) -> Result<Option<InstalledRuntime>> {
    let Some(active) = discover(paths)? else {
        return Ok(None);
    };
    verify_installation_at(active.root(), None).map(Some)
}

pub fn component_path(paths: &Paths, name: &str) -> Result<Option<PathBuf>> {
    discover(paths)?
        .map(|installation| installation.component(name))
        .transpose()
}

/// Prefer a verified managed component while preserving the legacy bundle
/// location used by Nix and development installations.
pub fn preferred_component(
    paths: &Paths,
    override_variable: &str,
    name: &str,
    legacy: &Path,
) -> Result<PathBuf> {
    if std::env::var_os(override_variable).is_some() {
        return Ok(legacy.to_path_buf());
    }
    match component_path(paths, name)? {
        Some(path) => Ok(path),
        None if option_env!("SPAWNR_RUNTIME_LOCK_JSON").is_some() => {
            bail!("managed runtime is not installed; run `spawnr setup`")
        }
        None => Ok(legacy.to_path_buf()),
    }
}

fn load_runtime_lock(path: Option<&Path>) -> Result<RuntimeLock> {
    let bytes = if let Some(path) = path {
        read_limited(path, MAX_LOCK_BYTES)
            .with_context(|| format!("read runtime lock {}", path.display()))?
    } else if let Some(embedded) = option_env!("SPAWNR_RUNTIME_LOCK_JSON") {
        embedded.as_bytes().to_vec()
    } else {
        bail!(
            "this development build has no embedded runtime lock; pass --runtime-lock and --runtime-archive"
        );
    };
    RuntimeLock::from_json(&bytes)
}

fn download_archive(paths: &Paths, lock: &RuntimeLock) -> Result<PathBuf> {
    let destination = paths.runtime_dir().join(format!(
        ".download-{}-{}",
        lock.runtime_version,
        Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut response = minreq::get(&lock.archive.url)
            .with_timeout(DOWNLOAD_TIMEOUT_SECONDS)
            .with_max_redirects(5)
            .send_lazy()
            .context("download managed runtime")?;
        ensure!(
            response.status_code == 200,
            "runtime download returned HTTP {} {}",
            response.status_code,
            response.reason_phrase
        );
        ensure!(
            response.url.starts_with("https://"),
            "runtime download redirected outside HTTPS"
        );
        if let Some(length) = response.headers.get("content-length") {
            ensure!(
                length.parse::<u64>().ok() == Some(lock.archive.size_bytes),
                "runtime download Content-Length does not match its lock"
            );
        }

        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&destination)
            .context("create temporary runtime download")?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut size = 0_u64;
        loop {
            let read = response
                .read(&mut buffer)
                .context("read runtime download")?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(read as u64)
                .context("runtime download size overflow")?;
            ensure!(
                size <= lock.archive.size_bytes,
                "runtime download exceeds its locked size"
            );
            output
                .write_all(&buffer[..read])
                .context("write temporary runtime download")?;
        }
        ensure!(
            size == lock.archive.size_bytes,
            "runtime download is truncated (received {size} bytes, expected {})",
            lock.archive.size_bytes
        );
        output.sync_all().context("sync runtime download")
    })();
    if result.is_err() {
        fs::remove_file(&destination).ok();
    }
    result.map(|()| destination)
}

fn verify_archive(path: &Path, lock: &RuntimeLock) -> Result<()> {
    let mut file = open_regular(path).context("open runtime archive")?;
    let (size, digest) = hash_reader(&mut file).context("hash runtime archive")?;
    ensure!(
        size == lock.archive.size_bytes,
        "runtime archive size is {size}, expected {}",
        lock.archive.size_bytes
    );
    ensure!(
        digest == lock.archive.sha256,
        "runtime archive SHA-256 does not match its lock"
    );
    Ok(())
}

fn extract_archive(path: &Path, staging: &Path, lock: &RuntimeLock) -> Result<()> {
    let file = open_regular(path)?;
    let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
        .context("open zstd runtime archive")?;
    let mut archive = tar::Archive::new(decoder);
    let mut actual = BTreeMap::new();
    let mut total_size = 0_u64;

    for entry in archive.entries().context("read runtime tar archive")? {
        let mut entry = entry.context("read runtime tar entry")?;
        ensure!(
            entry.header().entry_type().is_file(),
            "runtime archive contains a non-regular member"
        );
        let path = entry.path().context("decode runtime archive path")?;
        let path = path
            .to_str()
            .context("runtime archive path is not UTF-8")?
            .to_owned();
        if path != "manifest.json" {
            validate_runtime_path(&path, false)?;
        }
        ensure!(
            !actual.contains_key(&path),
            "duplicate runtime archive member {path}"
        );
        ensure!(
            entry.header().uid()? == 0 && entry.header().gid()? == 0,
            "runtime archive member {path} is not owned by 0:0"
        );
        ensure!(
            entry.header().mtime()? == 1,
            "runtime archive member {path} has a non-normalized mtime"
        );
        let mode = entry.header().mode()? & 0o7777;
        ensure!(
            mode == 0o444 || mode == 0o555,
            "runtime archive member {path} has unsafe mode {mode:o}"
        );
        let size = entry.header().size()?;
        ensure!(size > 0, "runtime archive member {path} is empty");
        total_size = total_size
            .checked_add(size)
            .context("runtime unpacked size overflow")?;
        ensure!(
            total_size <= MAX_UNPACKED_BYTES,
            "runtime archive exceeds the unpacked size limit"
        );
        if path == "manifest.json" {
            ensure!(
                size <= MAX_MANIFEST_BYTES,
                "runtime manifest exceeds the size limit"
            );
        }

        let destination = staging.join(&path);
        let parent = destination
            .parent()
            .context("runtime archive member has no parent")?;
        fs::create_dir_all(parent).context("create runtime archive directory")?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&destination)
            .with_context(|| format!("create runtime file {}", destination.display()))?;
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = entry.read(&mut buffer).context("read runtime member")?;
            if read == 0 {
                break;
            }
            written += read as u64;
            ensure!(
                written <= size,
                "runtime member {path} exceeds its header size"
            );
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .context("write runtime member")?;
        }
        ensure!(written == size, "runtime member {path} is truncated");
        output
            .set_permissions(fs::Permissions::from_mode(mode))
            .context("set runtime member permissions")?;
        output.sync_all().context("sync runtime member")?;
        actual.insert(
            path,
            ActualFile {
                size_bytes: size,
                sha256: hex_digest(hasher.finalize()),
                executable: mode == 0o555,
            },
        );
    }

    let manifest_path = staging.join("manifest.json");
    let manifest_bytes = read_limited(&manifest_path, MAX_MANIFEST_BYTES)
        .context("read extracted runtime manifest")?;
    let manifest_sha256 = hex_digest(Sha256::digest(&manifest_bytes));
    ensure!(
        manifest_sha256 == lock.archive.manifest_sha256,
        "runtime manifest SHA-256 does not match its lock"
    );
    let manifest = RuntimeManifest::from_json(&manifest_bytes)?;
    manifest.validate_against_lock(lock)?;
    let manifest_actual = actual
        .remove("manifest.json")
        .context("runtime archive has no manifest.json")?;
    ensure!(
        !manifest_actual.executable,
        "runtime manifest must not be executable"
    );
    ensure!(
        actual.len() == manifest.files.len(),
        "runtime archive members do not match the manifest"
    );
    for expected in &manifest.files {
        let found = actual
            .get(&expected.path)
            .with_context(|| format!("runtime archive is missing {}", expected.path))?;
        ensure!(
            found.size_bytes == expected.size_bytes
                && found.sha256 == expected.sha256
                && found.executable == expected.executable,
            "runtime archive member {} does not match its manifest",
            expected.path
        );
    }
    normalize_directories(staging)?;
    sync_tree(staging)
}

fn install_staging(
    paths: &Paths,
    staging: &Path,
    destination: &Path,
    installation: &InstalledRuntime,
) -> Result<()> {
    let signal_block = crate::process::TerminationSignalBlock::acquire()
        .context("protect runtime installation commit")?;
    if fs::symlink_metadata(destination).is_ok() {
        rename_exchange(staging, destination).context("atomically replace managed runtime")?;
    } else {
        fs::rename(staging, destination).context("atomically install managed runtime")?;
    }
    let committed = InstalledRuntime {
        root: destination.to_path_buf(),
        manifest: installation.manifest.clone(),
        manifest_sha256: installation.manifest_sha256.clone(),
    };
    write_active(paths, &committed)?;
    File::open(paths.runtime_dir())
        .and_then(|directory| directory.sync_all())
        .context("sync managed runtime directory")?;
    drop(signal_block);
    if staging.exists() {
        remove_any(staging).context("remove replaced managed runtime")?;
    }
    Ok(())
}

fn load_installation(root: &Path) -> Result<InstalledRuntime> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect managed runtime {}", root.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "managed runtime root is not a directory: {}",
        root.display()
    );
    let manifest_bytes = read_limited(&root.join("manifest.json"), MAX_MANIFEST_BYTES)
        .context("read installed runtime manifest")?;
    let manifest_sha256 = hex_digest(Sha256::digest(&manifest_bytes));
    let manifest = RuntimeManifest::from_json(&manifest_bytes)?;
    Ok(InstalledRuntime {
        root: root.to_path_buf(),
        manifest,
        manifest_sha256,
    })
}

fn verify_installation_at(root: &Path, lock: Option<&RuntimeLock>) -> Result<InstalledRuntime> {
    let installation = load_installation(root)?;
    if let Some(lock) = lock {
        ensure!(
            installation.manifest_sha256 == lock.archive.manifest_sha256,
            "installed runtime manifest digest does not match its lock"
        );
        installation.manifest.validate_against_lock(lock)?;
    }
    let manifest_metadata = fs::symlink_metadata(root.join("manifest.json"))?;
    ensure!(
        manifest_metadata.file_type().is_file()
            && manifest_metadata.nlink() == 1
            && manifest_metadata.mode() & 0o7777 == 0o444,
        "installed runtime manifest has unsafe metadata"
    );
    let expected: BTreeSet<_> = installation
        .manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    let mut found = BTreeSet::new();
    walk_runtime_files(root, root, &mut found)?;
    found.remove("manifest.json");
    ensure!(
        found == expected,
        "installed runtime files do not match its manifest"
    );
    for file in &installation.manifest.files {
        let path = root.join(&file.path);
        verify_runtime_file(&path, file)?;
    }
    Ok(installation)
}

fn verify_runtime_file(path: &Path, expected: &crate::runtime::RuntimeFile) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_file(),
        "{} is not a file",
        expected.path
    );
    ensure!(metadata.nlink() == 1, "{} is hard-linked", expected.path);
    ensure!(
        metadata.len() == expected.size_bytes,
        "{} has the wrong size",
        expected.path
    );
    let expected_mode = if expected.executable { 0o555 } else { 0o444 };
    ensure!(
        metadata.mode() & 0o7777 == expected_mode,
        "{} has unsafe permissions",
        expected.path
    );
    let mut input = open_regular(path)?;
    let (_, digest) = hash_reader(&mut input)?;
    ensure!(
        digest == expected.sha256,
        "{} failed SHA-256 verification",
        expected.path
    );
    Ok(())
}

fn walk_runtime_files<'a>(
    root: &'a Path,
    directory: &'a Path,
    found: &mut BTreeSet<String>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_dir() {
            walk_runtime_files(root, &entry.path(), found)?;
        } else {
            ensure!(
                metadata.file_type().is_file(),
                "managed runtime contains a special file"
            );
            let relative = entry.path().strip_prefix(root)?.to_path_buf();
            let relative = relative
                .to_str()
                .context("managed runtime path is not UTF-8")?
                .to_owned();
            ensure!(found.insert(relative), "duplicate managed runtime path");
        }
    }
    Ok(())
}

fn write_active(paths: &Paths, installation: &InstalledRuntime) -> Result<()> {
    let active = ActiveRuntime {
        schema_version: ACTIVE_SCHEMA_VERSION,
        runtime_version: installation.version().to_owned(),
        manifest_sha256: installation.manifest_sha256.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&active).context("encode active runtime")?;
    let destination = paths.runtime_active_file();
    let temporary = paths
        .runtime_dir()
        .join(format!(".active-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)
            .context("create active runtime file")?;
        output.write_all(&bytes)?;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::rename(&temporary, &destination).context("activate managed runtime")?;
        File::open(paths.runtime_dir())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

fn normalize_directories(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    collect_directories(root, &mut directories)?;
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555))?;
    }
    Ok(())
}

fn collect_directories(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    output.push(directory.to_path_buf());
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if fs::symlink_metadata(entry.path())?.file_type().is_dir() {
            collect_directories(&entry.path(), output)?;
        }
    }
    Ok(())
}

fn sync_tree(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    collect_directories(root, &mut directories)?;
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn rename_exchange(left: &Path, right: &Path) -> Result<()> {
    let left = CString::new(left.as_os_str().as_bytes()).context("runtime path contains NUL")?;
    let right = CString::new(right.as_os_str().as_bytes()).context("runtime path contains NUL")?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error()).context("renameat2(RENAME_EXCHANGE)")
    }
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            make_tree_removable(path)?;
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn make_tree_removable(directory: &Path) -> io::Result<()> {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if fs::symlink_metadata(entry.path())?.file_type().is_dir() {
            make_tree_removable(&entry.path())?;
        }
    }
    Ok(())
}

fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open regular file {}", path.display()))?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(file)
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file is not regular or exceeds its size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn hash_reader(reader: &mut impl Read) -> Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("file size overflow")?;
        hasher.update(&buffer[..read]);
    }
    Ok((size, hex_digest(hasher.finalize())))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes.as_ref() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("write to String");
    }
    output
}

impl SetupLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .context("open runtime setup lock")?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(io::Error::last_os_error()).context("lock runtime setup");
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ArchiveFormat, CliCompatibility, ComponentKind, RUNTIME_LOCK_SCHEMA_VERSION,
        RUNTIME_MANIFEST_SCHEMA_VERSION, RUNTIME_TARGET, RuntimeArchive, RuntimeComponent,
        RuntimeFile, RuntimeLauncher,
    };

    struct Fixture {
        root: PathBuf,
        paths: Paths,
        lock_path: PathBuf,
        archive_path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("spawnr-runtime-install-test-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            let paths = Paths::discover(Some(&root.join("data"))).unwrap();
            let archive_name = "spawnr-runtime-0.1.0-x86_64-linux.tar.zst";
            let archive_path = root.join(archive_name);
            let lock_path = root.join("runtime.lock.json");
            let (manifest, contents) = test_manifest();
            let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
            write_test_archive(&archive_path, &manifest_bytes, &contents);
            let mut archive = open_regular(&archive_path).unwrap();
            let (size_bytes, sha256) = hash_reader(&mut archive).unwrap();
            let lock = RuntimeLock {
                schema_version: RUNTIME_LOCK_SCHEMA_VERSION,
                runtime_version: "0.1.0".into(),
                target: RUNTIME_TARGET.into(),
                protocol_version: spawnr_protocol::PROTOCOL_VERSION,
                cli_compatibility: test_compatibility(),
                release_tag: "v0.1.0".into(),
                archive: RuntimeArchive {
                    file_name: archive_name.into(),
                    format: ArchiveFormat::TarZstd,
                    url: format!("https://example.invalid/{archive_name}"),
                    size_bytes,
                    sha256,
                    manifest_sha256: hex_digest(Sha256::digest(&manifest_bytes)),
                },
            };
            let mut lock_file = File::create(&lock_path).unwrap();
            serde_json::to_writer_pretty(&mut lock_file, &lock).unwrap();
            lock_file.write_all(b"\n").unwrap();
            Self {
                root,
                paths,
                lock_path,
                archive_path,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            remove_any(&self.root).unwrap();
        }
    }

    #[test]
    fn installs_verifies_and_reuses_an_offline_runtime() {
        let fixture = Fixture::new();
        let first = setup(
            &fixture.paths,
            Some(&fixture.lock_path),
            Some(&fixture.archive_path),
        )
        .unwrap();
        assert!(first.installed);
        assert_eq!(first.installation.version(), "0.1.0");
        assert!(first.installation.component("passt").unwrap().is_file());

        let active = verify_active(&fixture.paths).unwrap().unwrap();
        assert_eq!(
            active.manifest_sha256(),
            first.installation.manifest_sha256()
        );

        let second = setup(
            &fixture.paths,
            Some(&fixture.lock_path),
            Some(&fixture.archive_path),
        )
        .unwrap();
        assert!(!second.installed);
        assert_eq!(second.installation.root(), first.installation.root());
    }

    #[test]
    fn repairs_a_corrupted_same_version_installation() {
        let fixture = Fixture::new();
        let installed = setup(
            &fixture.paths,
            Some(&fixture.lock_path),
            Some(&fixture.archive_path),
        )
        .unwrap()
        .installation;
        let passt = installed.root().join("bin/passt");
        fs::set_permissions(&passt, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&passt, b"corrupt").unwrap();
        assert!(verify_active(&fixture.paths).is_err());

        let repaired = setup(
            &fixture.paths,
            Some(&fixture.lock_path),
            Some(&fixture.archive_path),
        )
        .unwrap();
        assert!(repaired.installed);
        verify_active(&fixture.paths).unwrap().unwrap();
        assert!(
            fs::read_dir(fixture.paths.runtime_dir())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".install-"))
        );
    }

    #[test]
    fn rejects_a_modified_archive_without_activating_it() {
        let fixture = Fixture::new();
        OpenOptions::new()
            .append(true)
            .open(&fixture.archive_path)
            .unwrap()
            .write_all(b"tamper")
            .unwrap();
        let error = setup(
            &fixture.paths,
            Some(&fixture.lock_path),
            Some(&fixture.archive_path),
        )
        .unwrap_err();
        assert!(error.to_string().contains("size"));
        assert!(discover(&fixture.paths).unwrap().is_none());
    }

    #[test]
    fn rejects_an_unsafe_active_runtime_pointer() {
        let fixture = Fixture::new();
        fixture.paths.ensure_layout().unwrap();
        fs::write(
            fixture.paths.runtime_active_file(),
            format!(
                "{{\"schema_version\":1,\"runtime_version\":\"../escape\",\"manifest_sha256\":\"{}\"}}",
                "a".repeat(64)
            ),
        )
        .unwrap();
        assert!(discover(&fixture.paths).is_err());
    }

    fn test_manifest() -> (RuntimeManifest, BTreeMap<String, Vec<u8>>) {
        let definitions = [
            ("busybox", "guest/busybox", ComponentKind::GuestExecutable),
            (
                "cloud-hypervisor",
                "bin/cloud-hypervisor",
                ComponentKind::HostExecutable,
            ),
            ("du", "bin/du", ComponentKind::HostExecutable),
            ("e2fsck", "bin/e2fsck", ComponentKind::HostExecutable),
            ("fuse2fs", "bin/fuse2fs", ComponentKind::HostExecutable),
            (
                "fusermount3",
                "bin/fusermount3",
                ComponentKind::HostExecutable,
            ),
            (
                "guest-initramfs",
                "guest/initramfs",
                ComponentKind::GuestInitramfs,
            ),
            ("guest-kernel", "guest/vmlinux", ComponentKind::GuestKernel),
            ("mkfs-ext4", "bin/mkfs.ext4", ComponentKind::HostExecutable),
            ("passt", "bin/passt", ComponentKind::HostExecutable),
            ("skopeo", "bin/skopeo", ComponentKind::HostExecutable),
            (
                "spawnr-agent",
                "guest/spawnr-agent",
                ComponentKind::GuestExecutable,
            ),
            ("umoci", "bin/umoci", ComponentKind::HostExecutable),
            ("unshare", "bin/unshare", ComponentKind::HostExecutable),
        ];
        let mut contents = BTreeMap::new();
        let mut components = Vec::new();
        for (name, path, kind) in definitions {
            contents.insert(path.to_owned(), format!("fixture:{name}").into_bytes());
            components.push(RuntimeComponent {
                name: name.into(),
                version: "test".into(),
                kind,
                path: path.into(),
                launcher: (kind == ComponentKind::HostExecutable)
                    .then_some(RuntimeLauncher::Direct),
            });
        }
        let files = contents
            .iter()
            .map(|(path, bytes)| {
                let executable = components.iter().any(|component| {
                    component.path == *path
                        && matches!(
                            component.kind,
                            ComponentKind::HostExecutable | ComponentKind::GuestExecutable
                        )
                });
                RuntimeFile {
                    path: path.clone(),
                    size_bytes: bytes.len() as u64,
                    sha256: hex_digest(Sha256::digest(bytes)),
                    executable,
                }
            })
            .collect();
        let manifest = RuntimeManifest {
            schema_version: RUNTIME_MANIFEST_SCHEMA_VERSION,
            runtime_version: "0.1.0".into(),
            target: RUNTIME_TARGET.into(),
            protocol_version: spawnr_protocol::PROTOCOL_VERSION,
            cli_compatibility: test_compatibility(),
            components,
            files,
        };
        manifest.validate().unwrap();
        (manifest, contents)
    }

    fn test_compatibility() -> CliCompatibility {
        CliCompatibility {
            minimum: "0.1.0".into(),
            maximum_exclusive: "0.2.0".into(),
        }
    }

    fn write_test_archive(
        path: &Path,
        manifest_bytes: &[u8],
        contents: &BTreeMap<String, Vec<u8>>,
    ) {
        let output = File::create(path).unwrap();
        let encoder = zstd::stream::write::Encoder::new(output, 1).unwrap();
        let mut archive = tar::Builder::new(encoder);
        append_test_member(&mut archive, "manifest.json", manifest_bytes, false);
        for (path, bytes) in contents {
            let executable = path.starts_with("bin/")
                || matches!(path.as_str(), "guest/busybox" | "guest/spawnr-agent");
            append_test_member(&mut archive, path, bytes, executable);
        }
        archive.finish().unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }

    fn append_test_member<W: Write>(
        archive: &mut tar::Builder<W>,
        path: &str,
        bytes: &[u8],
        executable: bool,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1);
        header.set_mode(if executable { 0o555 } else { 0o444 });
        header.set_cksum();
        archive.append(&header, bytes).unwrap();
    }
}
