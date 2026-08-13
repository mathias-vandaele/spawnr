//! Persistent storage layout for a Spawnr machine.
//!
//! The directory tree deliberately mirrors Spawnr's security model. The
//! environment and workspace are separate block images; runtime/session files
//! live in a third directory which is discarded between boots. Code which
//! publishes an environment therefore has one precise input (environment.raw)
//! and never needs to filter workspace or credential paths.

use crate::paths::{Paths, create_private_dir};
use crate::state::{MachineRecord, OwnershipMarker};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub const SPAWNR_OWNER: &str = "spawnr";
const OWNER_FILE: &str = "owner.json";
const DOMAIN_FILE: &str = "domain.json";
const MIN_EXT4_BYTES: u64 = 16 * 1024 * 1024;
const FICLONE: libc::c_ulong = 0x4004_9409;

/// Default sparse image capacities. Actual host allocation grows on demand.
pub const DEFAULT_ENVIRONMENT_BYTES: u64 = 32 * 1024 * 1024 * 1024;
pub const DEFAULT_WORKSPACE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSizes {
    pub environment_bytes: u64,
    pub workspace_bytes: u64,
}

impl Default for DiskSizes {
    fn default() -> Self {
        Self {
            environment_bytes: DEFAULT_ENVIRONMENT_BYTES,
            workspace_bytes: DEFAULT_WORKSPACE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StorageDomain {
    Environment,
    Workspace,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DomainMarker {
    owner: String,
    machine_id: Uuid,
    workspace_id: Uuid,
    domain: StorageDomain,
}

/// All host paths owned by one machine.
///
/// Paths are derived from the internal UUID, never the user-controlled machine
/// name. Every path remains beneath Spawnr's private machines directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachinePaths {
    pub root: PathBuf,
    pub owner_marker: PathBuf,

    /// Publishable persistent state. Nothing outside this directory is an
    /// input to environment publishing.
    pub environment_dir: PathBuf,
    pub environment_disk: PathBuf,

    /// Persistent private source state. This disk is mounted at /workspace in
    /// the guest and is never an input to publishing.
    pub workspace_dir: PathBuf,
    pub workspace_disk: PathBuf,

    /// Ephemeral host-side endpoints and identity capabilities. The guest
    /// counterpart is /run/spawnr on tmpfs.
    pub session_dir: PathBuf,
    pub vmm_pid_file: PathBuf,
    pub network_pid_file: PathBuf,
    pub api_socket: PathBuf,
    pub vsock_socket: PathBuf,
    pub network_socket: PathBuf,
    pub vmm_log: PathBuf,
    pub serial_log: PathBuf,
}

impl MachinePaths {
    pub fn new(paths: &Paths, record: &MachineRecord) -> Self {
        Self::for_record(paths, record)
    }

    pub fn for_record(paths: &Paths, record: &MachineRecord) -> Self {
        let root = paths.machine_dir(&record.id.to_string());
        let environment_dir = root.join("environment");
        let workspace_dir = root.join("workspace");
        let session_dir = root.join("session");
        Self {
            owner_marker: root.join(OWNER_FILE),
            environment_disk: environment_dir.join("environment.raw"),
            workspace_disk: workspace_dir.join("workspace.raw"),
            vmm_pid_file: session_dir.join("cloud-hypervisor.pid.json"),
            network_pid_file: session_dir.join("passt.pid.json"),
            // Keep endpoint names short: sockaddr_un.sun_path is only 108
            // bytes and the data root plus internal UUID already consumes most
            // of that budget.
            api_socket: session_dir.join("api.sock"),
            vsock_socket: session_dir.join("vsock.sock"),
            network_socket: session_dir.join("net.sock"),
            vmm_log: session_dir.join("cloud-hypervisor.log"),
            serial_log: session_dir.join("serial.log"),
            root,
            environment_dir,
            workspace_dir,
            session_dir,
        }
    }

    /// Create a new, private machine tree and its ownership/domain markers.
    /// Existing paths are never adopted.
    pub fn create(&self, record: &MachineRecord) -> Result<()> {
        let parent = self
            .root
            .parent()
            .context("machine directory has no parent")?;
        assert_private_directory(parent, "Spawnr machines directory")?;

        fs::create_dir(&self.root).with_context(|| {
            format!(
                "create machine directory {} (refusing to adopt an existing path)",
                self.root.display()
            )
        })?;
        let result = (|| -> Result<()> {
            set_private_permissions(&self.root)?;
            create_private_dir(&self.environment_dir)?;
            create_private_dir(&self.workspace_dir)?;
            create_private_dir(&self.session_dir)?;
            self.write_marker(record)?;
            self.write_domain_markers(record)?;
            sync_directory(&self.root)?;
            sync_directory(parent)?;
            Ok(())
        })();

        if result.is_err() {
            // root was created by this call, so cleanup cannot target a
            // pre-existing user directory.
            let _ = fs::remove_dir_all(&self.root);
        }
        result
    }

    pub fn write_marker(&self, record: &MachineRecord) -> Result<()> {
        let marker = OwnershipMarker {
            owner: SPAWNR_OWNER.to_owned(),
            machine_id: record.id,
            machine_name: record.name.clone(),
        };
        write_json_new(&self.owner_marker, &marker)
            .with_context(|| format!("write ownership marker for {:?}", record.name))
    }

    /// Prove that this tree belongs to precisely record before mutation or
    /// deletion. A name match alone is intentionally insufficient.
    pub fn assert_owned(&self, record: &MachineRecord) -> Result<()> {
        assert_private_directory(&self.root, "machine directory")?;
        let marker: OwnershipMarker = read_json_regular(&self.owner_marker)?;
        ensure!(
            marker.owner == SPAWNR_OWNER
                && marker.machine_id == record.id
                && marker.machine_name == record.name,
            "refusing to manage {}: ownership marker does not match machine {:?}",
            self.root.display(),
            record.name
        );
        Ok(())
    }

    /// Validate the three structural storage domains. This is stricter than
    /// assert_owned and is used before boot/publish; removal only needs the
    /// root ownership proof so a partially damaged machine remains removable.
    pub fn assert_domain_layout(&self, record: &MachineRecord) -> Result<()> {
        self.assert_owned(record)?;
        for (directory, domain) in [
            (&self.environment_dir, StorageDomain::Environment),
            (&self.workspace_dir, StorageDomain::Workspace),
            (&self.session_dir, StorageDomain::Session),
        ] {
            assert_private_directory(directory, "machine storage domain")?;
            let marker: DomainMarker = read_json_regular(&directory.join(DOMAIN_FILE))?;
            ensure!(
                marker
                    == (DomainMarker {
                        owner: SPAWNR_OWNER.to_owned(),
                        machine_id: record.id,
                        workspace_id: record.workspace_id,
                        domain,
                    }),
                "storage domain marker does not match {}",
                directory.display()
            );
        }
        Ok(())
    }

    /// Discard every session capability and runtime endpoint without touching
    /// either persistent disk.
    pub fn clear_session(&self, record: &MachineRecord) -> Result<()> {
        self.assert_owned(record)?;
        if let Ok(metadata) = fs::symlink_metadata(&self.session_dir) {
            ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "refusing to clear non-directory session path {}",
                self.session_dir.display()
            );
            fs::remove_dir_all(&self.session_dir)
                .with_context(|| format!("clear session {}", self.session_dir.display()))?;
        }
        create_private_dir(&self.session_dir)?;
        self.write_domain_marker(record, StorageDomain::Session, &self.session_dir)?;
        sync_directory(&self.root)
    }

    /// Remove only a tree carrying Spawnr's exact UUID/name marker.
    pub fn remove(&self, record: &MachineRecord) -> Result<()> {
        self.assert_owned(record)?;
        let parent = self
            .root
            .parent()
            .context("machine directory has no parent")?;
        fs::remove_dir_all(&self.root)
            .with_context(|| format!("remove Spawnr machine {}", self.root.display()))?;
        sync_directory(parent)
    }

    fn write_domain_markers(&self, record: &MachineRecord) -> Result<()> {
        self.write_domain_marker(record, StorageDomain::Environment, &self.environment_dir)?;
        self.write_domain_marker(record, StorageDomain::Workspace, &self.workspace_dir)?;
        self.write_domain_marker(record, StorageDomain::Session, &self.session_dir)
    }

    fn write_domain_marker(
        &self,
        record: &MachineRecord,
        domain: StorageDomain,
        directory: &Path,
    ) -> Result<()> {
        write_json_new(
            &directory.join(DOMAIN_FILE),
            &DomainMarker {
                owner: SPAWNR_OWNER.to_owned(),
                machine_id: record.id,
                workspace_id: record.workspace_id,
                domain,
            },
        )
    }
}

/// Materialize the requested OCI environment in the immutable host cache.
///
/// This may perform slow registry and image-conversion work, but it does not
/// create machine-owned storage or attach the cache image to a VM.
pub fn prepare_machine_environment(
    paths: &Paths,
    record: &mut MachineRecord,
    verbose: u8,
) -> Result<PathBuf> {
    paths.ensure_layout()?;
    let base_environment = crate::oci::prepare_environment(paths, record, verbose)
        .context("prepare OCI environment filesystem")?;
    let metadata = crate::oci::prepared_metadata(&base_environment)
        .context("read prepared OCI environment identity")?;
    record.environment.manifest_digest = Some(metadata.source_digest);
    record.environment.base_cache_key = Some(metadata.cache_key);
    Ok(base_environment)
}

/// Clone a prepared cache image into a machine-owned writable environment and
/// create its independent workspace disk. Both domains are removed if either
/// operation fails, so the caller can commit state only after this succeeds.
pub fn commit_machine_storage(
    paths: &Paths,
    record: &MachineRecord,
    base_environment: &Path,
) -> Result<()> {
    let machine = MachinePaths::for_record(paths, record);
    machine.create(record)?;
    let result = Storage::discover().and_then(|storage| {
        storage.create(
            &machine,
            record,
            Some(base_environment),
            DiskSizes::default(),
        )
    });
    if result.is_err() {
        let _ = machine.remove(record);
    }
    result
}

pub fn remove_machine(paths: &Paths, record: &MachineRecord) -> Result<()> {
    // A crashed VMM may leave passt alive even though the machine appears
    // stopped. Stop every identity-owned runtime helper before deleting the
    // session directory which contains the only safe handle to that process.
    crate::vmm::stop(paths, record, 0).context("stop machine runtime before removing storage")?;
    MachinePaths::for_record(paths, record).remove(record)
}

/// Creates raw ext4 block images without mounting them on the host.
#[derive(Debug, Clone)]
pub struct Storage {
    mkfs_ext4: PathBuf,
}

impl Storage {
    pub fn discover() -> Result<Self> {
        let mkfs_ext4 = if let Some(path) = env::var_os("SPAWNR_MKFS_EXT4") {
            executable(PathBuf::from(path), "SPAWNR_MKFS_EXT4")?
        } else {
            find_executable("mkfs.ext4")
                .context("mkfs.ext4 is required to create machine disks (install e2fsprogs)")?
        };
        Ok(Self { mkfs_ext4 })
    }

    pub fn with_mkfs_ext4(path: impl Into<PathBuf>) -> Self {
        Self {
            mkfs_ext4: path.into(),
        }
    }

    pub fn mkfs_ext4(&self) -> &Path {
        &self.mkfs_ext4
    }

    /// Create both persistent domains. If base_environment is provided it is
    /// cloned (reflink where supported) into an independent writable image.
    pub fn create(
        &self,
        paths: &MachinePaths,
        record: &MachineRecord,
        base_environment: Option<&Path>,
        sizes: DiskSizes,
    ) -> Result<()> {
        paths.assert_domain_layout(record)?;
        ensure!(
            !paths.environment_disk.exists() && !paths.workspace_disk.exists(),
            "refusing to overwrite existing machine storage"
        );

        let result = (|| -> Result<()> {
            if let Some(base) = base_environment {
                validate_ext4_image(base).with_context(|| {
                    format!(
                        "environment base {} is not a raw ext4 image",
                        base.display()
                    )
                })?;
                clone_image_atomic(base, &paths.environment_disk)?;
            } else {
                self.create_ext4_atomic(
                    &paths.environment_disk,
                    sizes.environment_bytes,
                    "SPAWNR_ENV",
                )?;
            }
            self.create_ext4_atomic(
                &paths.workspace_disk,
                sizes.workspace_bytes,
                "SPAWNR_WORKSPACE",
            )?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&paths.environment_disk);
            let _ = fs::remove_file(&paths.workspace_disk);
        }
        result
    }

    pub fn create_ext4(&self, destination: &Path, bytes: u64, label: &str) -> Result<()> {
        self.create_ext4_atomic(destination, bytes, label)
    }

    fn create_ext4_atomic(&self, destination: &Path, bytes: u64, label: &str) -> Result<()> {
        ensure!(
            bytes >= MIN_EXT4_BYTES,
            "ext4 image must be at least {MIN_EXT4_BYTES} bytes"
        );
        ensure_valid_label(label)?;
        ensure_destination_absent(destination)?;
        let temporary = temporary_sibling(destination);
        let result = (|| -> Result<()> {
            let file = open_new_regular(&temporary)?;
            file.set_len(bytes)
                .with_context(|| format!("size sparse image {}", temporary.display()))?;
            drop(file);

            let status = Command::new(&self.mkfs_ext4)
                .arg("-q")
                .arg("-F")
                .arg("-m")
                .arg("0")
                .arg("-L")
                .arg(label)
                .arg("--")
                .arg(&temporary)
                .status()
                .with_context(|| format!("run {}", self.mkfs_ext4.display()))?;
            ensure!(
                status.success(),
                "{} failed while formatting {} ({status})",
                self.mkfs_ext4.display(),
                destination.display()
            );
            validate_ext4_image(&temporary)?;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            File::open(&temporary)?.sync_all()?;
            commit_no_replace(&temporary, destination)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// Verify the ext4 superblock magic of a raw, unpartitioned image.
pub fn validate_ext4_image(path: &Path) -> Result<()> {
    let mut file = open_existing_regular(path)?;
    // The ext superblock starts at byte 1024 and s_magic is at offset 0x38.
    file.seek(SeekFrom::Start(1024 + 0x38))?;
    let mut magic = [0_u8; 2];
    file.read_exact(&mut magic)
        .with_context(|| format!("read ext4 superblock from {}", path.display()))?;
    ensure!(
        u16::from_le_bytes(magic) == 0xEF53,
        "{} does not contain a raw ext4 filesystem",
        path.display()
    );
    Ok(())
}

fn clone_image_atomic(source: &Path, destination: &Path) -> Result<()> {
    ensure_destination_absent(destination)?;
    let temporary = temporary_sibling(destination);
    let result = (|| -> Result<()> {
        clone_sparse_or_reflink(source, &temporary)?;
        validate_ext4_image(&temporary)?;
        commit_no_replace(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn clone_sparse_or_reflink(source: &Path, destination: &Path) -> Result<()> {
    let source = open_existing_regular(source)?;
    let mut destination = open_new_regular(destination)?;

    // FICLONE is instantaneous on Btrfs/XFS and gives every machine an
    // independent copy-on-write environment disk.
    let cloned = unsafe { libc::ioctl(destination.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if cloned == 0 {
        destination.sync_all()?;
        return Ok(());
    }

    let clone_error = io::Error::last_os_error();
    match clone_error.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY) => {}
        _ => return Err(clone_error).context("reflink environment image"),
    }

    destination.set_len(0)?;
    sparse_copy(&source, &mut destination).context("copy environment image")?;
    destination.sync_all()?;
    Ok(())
}

fn sparse_copy(source: &File, destination: &mut File) -> Result<()> {
    let length = source.metadata()?.len();
    destination.set_len(length)?;
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];

    while offset < length {
        let data =
            unsafe { libc::lseek(source.as_raw_fd(), offset as libc::off_t, libc::SEEK_DATA) };
        if data < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP)) {
                // Filesystem does not report extents. Correctness takes
                // precedence; std's platform copy path is still bounded.
                let mut source = source.try_clone()?;
                source.seek(SeekFrom::Start(0))?;
                destination.seek(SeekFrom::Start(0))?;
                destination.set_len(0)?;
                io::copy(&mut source, destination)?;
                return Ok(());
            }
            return Err(error).context("locate data extent");
        }
        let data = data as u64;
        let hole = unsafe { libc::lseek(source.as_raw_fd(), data as libc::off_t, libc::SEEK_HOLE) };
        let hole = if hole < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENXIO) {
                length
            } else {
                return Err(error).context("locate sparse hole");
            }
        } else {
            (hole as u64).min(length)
        };
        ensure!(hole >= data, "filesystem returned an invalid sparse extent");

        let mut position = data;
        while position < hole {
            let count = ((hole - position) as usize).min(buffer.len());
            let read = source.read_at(&mut buffer[..count], position)?;
            ensure!(read != 0, "unexpected EOF while cloning raw image");
            write_all_at(destination, &buffer[..read], position)?;
            position += read as u64;
        }
        offset = hole;
    }
    Ok(())
}

fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> io::Result<()> {
    while !data.is_empty() {
        let written = file.write_at(data, offset)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write raw image"));
        }
        data = &data[written..];
        offset += written as u64;
    }
    Ok(())
}

fn open_existing_regular(path: &Path) -> Result<File> {
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

fn open_new_regular(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("create private file {}", path.display()))
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = open_new_regular(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    sync_parent(path)
}

fn read_json_regular<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = open_existing_regular(path)?;
    ensure!(
        file.metadata()?.mode() & 0o077 == 0,
        "insecure permissions on {}",
        path.display()
    );
    serde_json::from_reader(file).with_context(|| format!("read marker {}", path.display()))
}

fn assert_private_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {description} {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "{description} is not a real directory: {}",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "{description} is accessible by another user: {}",
        path.display()
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "{description} is not owned by the current user: {}",
        path.display()
    );
    Ok(())
}

fn set_private_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure {}", path.display()))
}

fn ensure_destination_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        Ok(_) => bail!("refusing to overwrite {}", path.display()),
    }
}

fn temporary_sibling(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", Uuid::new_v4()));
    destination.with_file_name(name)
}

fn commit_no_replace(temporary: &Path, destination: &Path) -> Result<()> {
    // link(2) commits atomically without rename(2)'s overwrite behavior. Both
    // names are siblings, so they are necessarily on the same filesystem.
    fs::hard_link(temporary, destination).with_context(|| {
        format!(
            "commit {} to {} without replacing an existing file",
            temporary.display(),
            destination.display()
        )
    })?;
    if let Err(error) = fs::remove_file(temporary) {
        let _ = fs::remove_file(destination);
        return Err(error)
            .with_context(|| format!("remove temporary image {}", temporary.display()));
    }
    if let Err(error) = sync_parent(destination) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    sync_directory(path.parent().context("path has no parent")?)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))
}

fn ensure_valid_label(label: &str) -> Result<()> {
    ensure!(!label.is_empty(), "filesystem label cannot be empty");
    ensure!(
        label.len() <= 16
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "invalid ext4 label {label:?}"
    );
    Ok(())
}

fn find_executable(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find_map(|candidate| executable(candidate, name).ok())
    })
}

fn executable(path: PathBuf, description: &str) -> Result<PathBuf> {
    let metadata = fs::metadata(&path)
        .with_context(|| format!("inspect {description} at {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{} is not a file",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o111 != 0,
        "{} is not executable",
        path.display()
    );
    fs::canonicalize(&path).with_context(|| format!("canonicalize {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EnvironmentRecord;
    use std::os::unix::fs::PermissionsExt;
    use time::OffsetDateTime;

    struct TestDir(PathBuf);

    impl TestDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record() -> MachineRecord {
        MachineRecord {
            id: Uuid::new_v4(),
            name: "test-machine".into(),
            environment: EnvironmentRecord {
                reference: "example.invalid/env:1".into(),
                manifest_digest: None,
                base_cache_key: None,
            },
            repository: None,
            repository_dir: None,
            workspace_id: Uuid::new_v4(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            vsock_cid: 4096,
            mac_address: "02:00:00:00:00:01".into(),
        }
    }

    fn layout() -> (TestDir, MachineRecord, MachinePaths) {
        let temporary =
            std::env::temp_dir().join(format!("spawnr-storage-test-{}", Uuid::new_v4()));
        fs::create_dir(&temporary).unwrap();
        let temporary = TestDir(temporary);
        let paths = Paths::discover(Some(temporary.path())).unwrap();
        paths.ensure_layout().unwrap();
        let record = record();
        let machine = MachinePaths::for_record(&paths, &record);
        machine.create(&record).unwrap();
        (temporary, record, machine)
    }

    #[test]
    fn layout_is_private_and_structurally_separated() {
        let (_temporary, record, paths) = layout();
        paths.assert_domain_layout(&record).unwrap();
        assert_ne!(
            paths.environment_disk.parent(),
            paths.workspace_disk.parent()
        );
        assert_ne!(
            paths.environment_disk.parent(),
            Some(paths.session_dir.as_path())
        );
        for directory in [
            &paths.root,
            &paths.environment_dir,
            &paths.workspace_dir,
            &paths.session_dir,
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn clearing_session_cannot_touch_persistent_domains() {
        let (_temporary, record, paths) = layout();
        fs::write(paths.environment_dir.join("keep"), b"environment").unwrap();
        fs::write(paths.workspace_dir.join("keep"), b"workspace").unwrap();
        fs::write(paths.session_dir.join("secret"), b"credential").unwrap();
        paths.clear_session(&record).unwrap();
        assert_eq!(
            fs::read(paths.environment_dir.join("keep")).unwrap(),
            b"environment"
        );
        assert_eq!(
            fs::read(paths.workspace_dir.join("keep")).unwrap(),
            b"workspace"
        );
        assert!(!paths.session_dir.join("secret").exists());
        paths.assert_domain_layout(&record).unwrap();
    }

    #[test]
    fn tampered_marker_blocks_destructive_removal() {
        let (_temporary, record, paths) = layout();
        fs::write(&paths.owner_marker, b"{}\n").unwrap();
        assert!(paths.remove(&record).is_err());
        assert!(paths.root.exists());
    }

    #[test]
    fn mkfs_creates_two_independent_raw_ext4_images() {
        let Some(mkfs) = find_executable("mkfs.ext4") else {
            return;
        };
        let (_temporary, record, paths) = layout();
        Storage::with_mkfs_ext4(mkfs)
            .create(
                &paths,
                &record,
                None,
                DiskSizes {
                    environment_bytes: MIN_EXT4_BYTES,
                    workspace_bytes: MIN_EXT4_BYTES,
                },
            )
            .unwrap();
        validate_ext4_image(&paths.environment_disk).unwrap();
        validate_ext4_image(&paths.workspace_disk).unwrap();
        assert_ne!(
            fs::metadata(&paths.environment_disk).unwrap().ino(),
            fs::metadata(&paths.workspace_disk).unwrap().ino()
        );
    }

    #[test]
    fn cached_environment_is_cloned_into_independent_writable_storage() {
        let Some(mkfs) = find_executable("mkfs.ext4") else {
            return;
        };
        let (temporary, record, paths) = layout();
        let storage = Storage::with_mkfs_ext4(mkfs);
        let base = temporary.path().join("base.raw");
        storage
            .create_ext4(&base, MIN_EXT4_BYTES, "SPAWNR_ENV")
            .unwrap();
        storage
            .create(
                &paths,
                &record,
                Some(&base),
                DiskSizes {
                    environment_bytes: MIN_EXT4_BYTES,
                    workspace_bytes: MIN_EXT4_BYTES,
                },
            )
            .unwrap();

        let destination = OpenOptions::new()
            .write(true)
            .open(&paths.environment_disk)
            .unwrap();
        destination.write_all_at(&[0x5a], 0).unwrap();
        let source = File::open(&base).unwrap();
        let mut source_byte = [0xff];
        source.read_exact_at(&mut source_byte, 0).unwrap();
        assert_eq!(source_byte, [0]);
        validate_ext4_image(&base).unwrap();
        validate_ext4_image(&paths.environment_disk).unwrap();
    }

    #[test]
    fn image_creation_never_replaces_an_existing_destination() {
        let temporary =
            std::env::temp_dir().join(format!("spawnr-storage-overwrite-test-{}", Uuid::new_v4()));
        fs::create_dir(&temporary).unwrap();
        let temporary = TestDir(temporary);
        let destination = temporary.path().join("existing.raw");
        fs::write(&destination, b"owned elsewhere").unwrap();
        let storage = Storage::with_mkfs_ext4("/bin/false");
        assert!(
            storage
                .create_ext4(&destination, MIN_EXT4_BYTES, "SPAWNR_ENV")
                .is_err()
        );
        assert_eq!(fs::read(destination).unwrap(), b"owned elsewhere");
    }
}
