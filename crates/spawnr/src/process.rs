//! Identity-safe management of long-lived helper processes.
//!
//! A numeric PID is not an identity: Linux may reuse it after a crash or host
//! reboot. Spawnr records the boot ID, procfs start time, and executable inode,
//! and revalidates all of them before sending a signal.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
const MAX_PID_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub boot_id: String,
    pub start_time_ticks: u64,
    pub executable: PathBuf,
    pub executable_device: u64,
    pub executable_inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedProcessStatus {
    Missing,
    Running(ProcessIdentity),
    Stale {
        recorded: ProcessIdentity,
        reason: String,
    },
}

impl ManagedProcessStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    pub fn identity(&self) -> Option<&ProcessIdentity> {
        match self {
            Self::Running(identity)
            | Self::Stale {
                recorded: identity, ..
            } => Some(identity),
            Self::Missing => None,
        }
    }
}

/// A just-spawned child retained while the caller performs readiness checks.
pub struct SpawnedProcess {
    pub child: Child,
    pub identity: ProcessIdentity,
}

impl ProcessIdentity {
    pub fn capture(pid: u32) -> Result<Self> {
        ensure!(pid > 0, "refusing invalid PID {pid}");
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
            .with_context(|| format!("read identity for process {pid}"))?;
        let start_time_ticks =
            parse_start_time(&stat).with_context(|| format!("parse identity for process {pid}"))?;
        let executable_link = PathBuf::from(format!("/proc/{pid}/exe"));
        let executable = fs::read_link(&executable_link)
            .with_context(|| format!("read executable for process {pid}"))?;
        let executable_metadata = fs::metadata(&executable_link)
            .with_context(|| format!("inspect executable for process {pid}"))?;
        let boot_id = read_boot_id()?;
        Ok(Self {
            pid,
            boot_id,
            start_time_ticks,
            executable,
            executable_device: executable_metadata.dev(),
            executable_inode: executable_metadata.ino(),
        })
    }

    /// Returns false for an exited process, a reused PID, or a prior host boot.
    pub fn is_current(&self) -> Result<bool> {
        self.matches_running_process()
    }

    fn matches_running_process(&self) -> Result<bool> {
        let stat = match fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("read managed process start time"),
        };
        let (state, start_time_ticks) = parse_state_and_start_time(&stat)?;
        if self.boot_id != read_boot_id()? || self.start_time_ticks != start_time_ticks {
            return Ok(false);
        }
        // A child spawned by an earlier operation in this same CLI process can
        // remain visible as a zombie until the top-level command exits. It has
        // already released every runtime resource and cannot handle signals;
        // treating it as live makes rollback wait forever (notably for
        // non-dumpable passt, whose /proc/<pid>/exe remains permission-denied).
        if matches!(state, 'Z' | 'X') {
            return Ok(false);
        }

        // Sandboxed helpers such as passt deliberately become non-dumpable,
        // at which point Linux denies /proc/<pid>/exe even to their owner.
        // Boot ID + kernel start time still form a non-reusable identity. Use
        // the executable inode as an additional check whenever procfs permits.
        match fs::metadata(format!("/proc/{}/exe", self.pid)) {
            Ok(metadata) => {
                Ok(self.executable_device == metadata.dev()
                    && self.executable_inode == metadata.ino())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(true),
            Err(error) => Err(error).context("inspect managed process executable"),
        }
    }
}

/// Search PATH for a regular executable. The returned path is canonical.
pub fn find_executable(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find_map(|candidate| validate_executable(&candidate).ok())
    })
}

/// Resolve an optional environment override, then a bundled path, then PATH.
pub fn resolve_executable(
    override_variable: &str,
    bundled: &Path,
    command_name: &str,
) -> Result<PathBuf> {
    if let Some(path) = env::var_os(override_variable) {
        return validate_executable(Path::new(&path))
            .with_context(|| format!("{override_variable} points to an unusable executable"));
    }
    if bundled.exists() {
        return validate_executable(bundled);
    }
    find_executable(command_name).with_context(|| {
        format!(
            "cannot find {command_name}; set {override_variable} or install it at {}",
            bundled.display()
        )
    })
}

pub fn validate_executable(path: &Path) -> Result<PathBuf> {
    let metadata =
        fs::metadata(path).with_context(|| format!("inspect executable {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o111 != 0,
        "{} is not executable",
        path.display()
    );
    fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
}

/// Spawn a detached process, send its output to a private log, and atomically
/// record its full Linux identity.
pub fn spawn_managed(
    command: &mut Command,
    pid_file: &Path,
    log_file: &Path,
) -> Result<SpawnedProcess> {
    ensure_pid_file_absent(pid_file)?;
    let log = open_private_log(log_file)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));

    // A termination signal between spawn(2) and the synced PID identity file
    // would create an ownerless detached helper. Block only that tiny commit
    // window; the child restores the caller's mask before exec.
    let signal_block =
        TerminationSignalBlock::acquire().context("protect managed process ownership commit")?;
    let child_signal_mask = signal_block.previous;
    // The VMM must survive the short-lived CLI and must not share its terminal
    // process group. umask protects any runtime sockets/files it creates.
    unsafe {
        command.pre_exec(move || {
            let status =
                libc::pthread_sigmask(libc::SIG_SETMASK, &child_signal_mask, std::ptr::null_mut());
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status));
            }
            libc::umask(0o077);
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {:?}", command.get_program()))?;
    let identity = match ProcessIdentity::capture(child.id()) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("capture spawned process identity");
        }
    };
    if let Err(error) = write_pid_file(pid_file, &identity) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("record spawned process identity");
    }
    Ok(SpawnedProcess { child, identity })
}

pub(crate) struct TerminationSignalBlock {
    previous: libc::sigset_t,
}

impl TerminationSignalBlock {
    pub(crate) fn acquire() -> Result<Self> {
        let mut selected = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset initializes selected.
        if unsafe { libc::sigemptyset(selected.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error()).context("initialize termination signal set");
        }
        // SAFETY: initialized by successful sigemptyset.
        let mut selected = unsafe { selected.assume_init() };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: selected is initialized and each signal is valid.
            if unsafe { libc::sigaddset(&mut selected, signal) } != 0 {
                return Err(io::Error::last_os_error()).context("add termination signal");
            }
        }
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: both sets point to valid storage; pthread_sigmask returns an
        // errno value directly rather than setting errno.
        let status =
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &selected, previous.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status)).context("block termination signals");
        }
        // SAFETY: successful pthread_sigmask initialized previous.
        Ok(Self {
            previous: unsafe { previous.assume_init() },
        })
    }
}

impl Drop for TerminationSignalBlock {
    fn drop(&mut self) {
        // SAFETY: previous was initialized by pthread_sigmask. Failure cannot
        // be reported from Drop; the normal platform path succeeds.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

pub fn inspect_pid_file(path: &Path) -> Result<ManagedProcessStatus> {
    let recorded = match read_pid_file(path)? {
        Some(identity) => identity,
        None => return Ok(ManagedProcessStatus::Missing),
    };

    if recorded.matches_running_process()? {
        Ok(ManagedProcessStatus::Running(recorded))
    } else {
        Ok(ManagedProcessStatus::Stale {
            recorded,
            reason: "PID belongs to a different process or host boot".into(),
        })
    }
}

pub fn write_pid_file(path: &Path, identity: &ProcessIdentity) -> Result<()> {
    ensure_pid_file_absent(path)?;
    let temporary = temporary_sibling(path);
    let result = (|| -> Result<()> {
        let bytes = serde_json::to_vec_pretty(identity)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| format!("create temporary PID file {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;

        // link(2) is an atomic no-replace commit; unlike rename(2), it cannot
        // accidentally replace an identity written by a concurrent start.
        fs::hard_link(&temporary, path)
            .with_context(|| format!("commit PID file {}", path.display()))?;
        fs::remove_file(&temporary)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn read_pid_file(path: &Path) -> Result<Option<ProcessIdentity>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open PID file {}", path.display()));
        }
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.file_type().is_file() && metadata.len() <= MAX_PID_FILE_BYTES,
        "invalid PID file {}",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "PID file is accessible by another user: {}",
        path.display()
    );
    let identity: ProcessIdentity = serde_json::from_reader(file)
        .with_context(|| format!("parse PID file {}", path.display()))?;
    ensure!(
        identity.pid > 0,
        "invalid process identity in {}",
        path.display()
    );
    Ok(Some(identity))
}

/// Remove a PID file only if it still names the expected process.
pub fn remove_pid_file(path: &Path, expected: &ProcessIdentity) -> Result<()> {
    match read_pid_file(path)? {
        None => return Ok(()),
        Some(found) if found == *expected => {}
        Some(_) => bail!(
            "refusing to remove changed process identity at {}",
            path.display()
        ),
    }
    fs::remove_file(path).with_context(|| format!("remove PID file {}", path.display()))?;
    sync_parent(path)
}

/// Reap a stale identity file. A running process is never touched.
pub fn remove_stale_pid_file(path: &Path) -> Result<bool> {
    match inspect_pid_file(path)? {
        ManagedProcessStatus::Missing => Ok(false),
        ManagedProcessStatus::Running(identity) => bail!(
            "refusing to remove PID file for running process {}",
            identity.pid
        ),
        ManagedProcessStatus::Stale { recorded, .. } => {
            remove_pid_file(path, &recorded)?;
            Ok(true)
        }
    }
}

/// Send a signal through pidfd when available. The identity is rechecked after
/// opening pidfd, closing the PID-reuse race before the signal.
pub fn send_signal_checked(identity: &ProcessIdentity, signal: libc::c_int) -> Result<bool> {
    let pidfd = pidfd_open(identity.pid)?;
    if let Some(pidfd) = pidfd {
        if !identity.matches_running_process()? {
            return Ok(false);
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error).context("signal managed process through pidfd");
    }

    // Old-kernel fallback. Spawnr targets modern KVM hosts, but retaining this
    // path yields a useful diagnostic rather than an unconditional failure.
    if !identity.is_current()? {
        return Ok(false);
    }
    let result = unsafe { libc::kill(identity.pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(true)
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error).context("signal managed process")
        }
    }
}

pub fn wait_for_exit(identity: &ProcessIdentity, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if !identity.is_current()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// TERM, bounded wait, then KILL. Signals are only sent to the recorded Linux
/// process identity, never merely to its numeric PID.
pub fn terminate(identity: &ProcessIdentity, term_timeout: Duration) -> Result<()> {
    if !send_signal_checked(identity, libc::SIGTERM)? {
        return Ok(());
    }
    if wait_for_exit(identity, term_timeout)? {
        return Ok(());
    }
    ensure!(
        send_signal_checked(identity, libc::SIGKILL)?,
        "managed process {} changed identity before SIGKILL",
        identity.pid
    );
    ensure!(
        wait_for_exit(identity, Duration::from_secs(2))?,
        "process {} did not exit after SIGKILL",
        identity.pid
    );
    Ok(())
}

/// Stop and reap a process spawned by this CLI invocation. Reaping through
/// `Child::try_wait` matters here: an unreaped child remains present as a
/// zombie in procfs, so identity-only waiting would mistake it for a process
/// which ignored SIGTERM.
pub fn terminate_spawned(process: &mut SpawnedProcess, term_timeout: Duration) -> Result<()> {
    if process.child.try_wait()?.is_some() {
        return Ok(());
    }
    ensure!(
        send_signal_checked(&process.identity, libc::SIGTERM)?,
        "spawned process {} changed identity before SIGTERM",
        process.identity.pid
    );
    if wait_for_child(&mut process.child, term_timeout)? {
        return Ok(());
    }
    ensure!(
        send_signal_checked(&process.identity, libc::SIGKILL)?,
        "spawned process {} changed identity before SIGKILL",
        process.identity.pid
    );
    ensure!(
        wait_for_child(&mut process.child, Duration::from_secs(2))?,
        "spawned process {} did not exit after SIGKILL",
        process.identity.pid
    );
    Ok(())
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn pidfd_open(pid: u32) -> Result<Option<OwnedFd>> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    if fd >= 0 {
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) }));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOSYS | libc::EINVAL) => Ok(None),
        Some(libc::ESRCH) => Ok(None),
        _ => Err(error).context("open pidfd"),
    }
}

fn parse_start_time(stat: &str) -> Result<u64> {
    parse_state_and_start_time(stat).map(|(_, start_time)| start_time)
}

fn parse_state_and_start_time(stat: &str) -> Result<(char, u64)> {
    // comm is parenthesized and may itself contain spaces or ')' characters.
    let close = stat
        .rfind(')')
        .context("process stat has no closing command delimiter")?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // fields[0] is field 3 (state); starttime is field 22.
    let state = fields
        .first()
        .and_then(|value| value.chars().next())
        .context("process stat does not contain state")?;
    let value = fields
        .get(19)
        .context("process stat does not contain start time")?;
    let start_time = value
        .parse()
        .context("process start time is not an integer")?;
    Ok((state, start_time))
}

fn read_boot_id() -> Result<String> {
    let boot_id = fs::read_to_string(BOOT_ID_PATH).context("read Linux boot ID")?;
    let boot_id = boot_id.trim().to_owned();
    ensure!(!boot_id.is_empty(), "Linux boot ID is empty");
    Ok(boot_id)
}

fn ensure_pid_file_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        Ok(_) => bail!("process identity already exists at {}", path.display()),
    }
}

fn open_private_log(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open process log {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.file_type().is_file(),
        "process log is not a regular file: {}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", Uuid::new_v4()));
    path.with_file_name(name)
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn tempdir() -> TestDir {
        let path = env::temp_dir().join(format!("spawnr-process-test-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        TestDir(path)
    }

    #[test]
    fn parses_start_time_when_comm_contains_parentheses_and_spaces() {
        let mut fields = vec!["S".to_owned()];
        fields.extend((4..=21).map(|field| field.to_string()));
        fields.push("987654".into());
        fields.push("23".into());
        let stat = format!("42 (odd ) process name) {}", fields.join(" "));
        assert_eq!(parse_start_time(&stat).unwrap(), 987654);
    }

    #[test]
    fn treats_zombie_stat_as_exited_without_losing_identity() {
        let mut fields = vec!["Z".to_owned()];
        fields.extend((4..=21).map(|field| field.to_string()));
        fields.push("987654".into());
        let stat = format!("42 (exited helper) {}", fields.join(" "));
        assert_eq!(parse_state_and_start_time(&stat).unwrap(), ('Z', 987654));
    }

    #[test]
    fn current_process_identity_is_live() {
        let identity = ProcessIdentity::capture(std::process::id()).unwrap();
        assert!(identity.is_current().unwrap());
        assert!(identity.start_time_ticks > 0);
    }

    #[test]
    fn pid_file_distinguishes_stale_identity() {
        let temporary = tempdir();
        let path = temporary.path().join("pid.json");
        let mut identity = ProcessIdentity::capture(std::process::id()).unwrap();
        identity.start_time_ticks += 1;
        write_pid_file(&path, &identity).unwrap();
        assert!(matches!(
            inspect_pid_file(&path).unwrap(),
            ManagedProcessStatus::Stale { .. }
        ));
        assert!(remove_stale_pid_file(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn checked_signal_refuses_mismatched_process() {
        let mut identity = ProcessIdentity::capture(std::process::id()).unwrap();
        identity.executable_inode = identity.executable_inode.wrapping_add(1);
        assert!(!send_signal_checked(&identity, 0).unwrap());
    }

    #[test]
    fn spawned_process_has_private_atomic_identity_file() {
        let temporary = tempdir();
        let pid_file = temporary.path().join("process.pid.json");
        let log_file = temporary.path().join("process.log");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let mut spawned = spawn_managed(&mut command, &pid_file, &log_file).unwrap();
        assert!(matches!(
            inspect_pid_file(&pid_file).unwrap(),
            ManagedProcessStatus::Running(_)
        ));
        assert_eq!(
            fs::metadata(&pid_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        terminate_spawned(&mut spawned, Duration::from_secs(1)).unwrap();
        assert!(wait_for_exit(&spawned.identity, Duration::from_secs(1)).unwrap());
        remove_pid_file(&pid_file, &spawned.identity).unwrap();
    }
}
