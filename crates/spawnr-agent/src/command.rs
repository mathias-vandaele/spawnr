use crate::account::User;
use anyhow::{Context, Result, bail, ensure};
use spawnr_protocol::{Response, StreamKind, read_stream_frame, write_json, write_stream_frame};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use vsock::VsockStream;

const MAX_CAPTURED_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_ARGV_ITEMS: usize = 4096;
const MAX_ARG_BYTES: usize = 1024 * 1024;
const MAX_ENV_ITEMS: usize = 512;

pub fn clone_repository(
    workspace: &Path,
    session_dir: &Path,
    user: &User,
    repository: &str,
    destination: &str,
) -> Result<Response> {
    crate::mount::assert_workspace_is_separate(workspace)?;
    validate_text("repository", repository, 64 * 1024)?;
    let destination = contained_path(workspace, destination, false)?;
    ensure!(
        !destination.exists(),
        "clone destination {} already exists",
        destination.display()
    );
    let parent = destination
        .parent()
        .context("clone destination has no parent")?;
    ensure!(
        parent.exists(),
        "clone destination parent {} does not exist",
        parent.display()
    );
    ensure_contained_existing(workspace, parent)?;

    let mut command = Command::new(find_git()?);
    command
        .arg("clone")
        .arg("--")
        .arg(repository)
        .arg(&destination);
    prepare(
        &mut command,
        user,
        session_dir,
        Some(workspace),
        &BTreeMap::new(),
    )?;
    let result = run_captured(command)?;
    if result.exit_code != 0 {
        bail!(
            "git clone exited with {}: {}",
            result.exit_code,
            result.stderr.trim_end()
        );
    }
    Ok(Response::Ok {
        message: format!("cloned repository into {}", destination.display()),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn exec(
    stream: &mut VsockStream,
    workspace: &Path,
    session_dir: &Path,
    user: &User,
    argv: Vec<String>,
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    tty: bool,
    rows: u16,
    cols: u16,
) -> Result<()> {
    crate::mount::assert_workspace_is_separate(workspace)?;
    validate_argv(&argv)?;
    validate_environment(&env)?;
    let cwd = cwd
        .as_deref()
        .map(|path| contained_path(workspace, path, true))
        .transpose()?
        .unwrap_or_else(|| workspace.to_owned());

    if tty {
        exec_tty(stream, session_dir, user, argv, cwd, env, rows, cols)
    } else {
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        prepare(&mut command, user, session_dir, Some(&cwd), &env)?;
        let result = run_captured(command)?;
        write_json(stream, &result.into_response())
    }
}

pub fn workspace_status(
    workspace: &Path,
    session_dir: &Path,
    user: &User,
    repository_path: &str,
) -> Result<Response> {
    crate::mount::assert_workspace_is_separate(workspace)?;
    let repository = contained_path(workspace, repository_path, true)?;
    let mut command = Command::new(find_git()?);
    command.args(["status", "--porcelain=v1", "--untracked-files=all"]);
    prepare(
        &mut command,
        user,
        session_dir,
        Some(&repository),
        &BTreeMap::new(),
    )?;
    let result = run_captured(command)?;
    if result.exit_code != 0 {
        bail!(
            "git status exited with {}: {}",
            result.exit_code,
            result.stderr.trim_end()
        );
    }
    let porcelain = result.stdout;
    Ok(Response::WorkspaceStatus {
        clean: porcelain.is_empty(),
        porcelain,
    })
}

pub fn request_shutdown() -> Result<()> {
    if std::process::id() == 1 {
        // SAFETY: as the guest's PID 1 this is the intended whole-VM poweroff.
        unsafe {
            libc::sync();
            libc::reboot(libc::RB_POWER_OFF);
        }
        return Err(std::io::Error::last_os_error()).context("power off guest");
    }
    let shutdown = ["/usr/bin/systemctl", "/bin/systemctl", "/sbin/poweroff"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .context("systemctl/poweroff is unavailable")?;
    let mut command = Command::new(shutdown);
    if shutdown.ends_with("systemctl") {
        command.args(["--no-block", "poweroff"]);
    }
    let status = command.status().context("request guest poweroff")?;
    ensure!(status.success(), "poweroff request failed with {status}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn exec_tty(
    stream: &mut VsockStream,
    session_dir: &Path,
    user: &User,
    argv: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    rows: u16,
    cols: u16,
) -> Result<()> {
    let window = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors and does not retain any
    // pointer passed here.
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &window,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("allocate interactive PTY");
    }
    // SAFETY: a successful openpty returned two independently owned fds.
    let master = unsafe { fs::File::from_raw_fd(master_fd) };
    let slave = unsafe { fs::File::from_raw_fd(slave_fd) };

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(&cwd);
    populate_environment(&mut command, user, session_dir, &env)?;
    command.uid(user.uid).gid(user.gid);
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: the closure calls only async-signal-safe operations, does not
    // allocate, and reports failures through a preconstructed OS error.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let (mut child, _tracking) =
        crate::reaper::spawn(&mut command).context("spawn interactive command")?;
    let pid = child.id() as libc::pid_t;
    drop(slave);

    write_json(&mut *stream, &Response::ExecReady)?;
    relay_pty(stream, &mut child, pid, master)
}

fn relay_pty(
    stream: &mut VsockStream,
    child: &mut std::process::Child,
    pid: libc::pid_t,
    mut master: fs::File,
) -> Result<()> {
    let mut output = [0_u8; 16 * 1024];
    loop {
        if child_exit_ready(pid, true)? {
            // The login shell is the session leader. Do not let background
            // descendants retain the PTY and keep `spawnr open` alive after
            // the shell has logged out.
            // SAFETY: negative pid targets only the child's process group.
            unsafe { libc::kill(-pid, libc::SIGHUP) };
            let status = child.wait().context("reap interactive command")?;
            drain_pty(&mut master, stream, &mut output)?;
            return write_stream_frame(stream, StreamKind::Exit, &exit_code(status).to_be_bytes());
        }

        let mut descriptors = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: descriptors points to two initialized pollfd values.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 100) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll interactive session");
        }

        if descriptors[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match master.read(&mut output) {
                Ok(0) => {}
                Ok(count) => {
                    write_stream_frame(&mut *stream, StreamKind::Stdout, &output[..count])?
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                Err(error) => return Err(error).context("read PTY output"),
            }
        }

        if descriptors[0].revents & libc::POLLIN != 0 {
            let frame = read_stream_frame(&mut *stream);
            let (kind, payload) = match frame {
                Ok(frame) => frame,
                Err(error) => {
                    // SAFETY: negative pid targets the child process group.
                    unsafe { libc::kill(-pid, libc::SIGHUP) };
                    let _ = child.wait();
                    return Err(error).context("read interactive input");
                }
            };
            match kind {
                StreamKind::Stdin => master.write_all(&payload).context("write PTY input")?,
                StreamKind::Resize => resize_pty(master.as_raw_fd(), &payload)?,
                StreamKind::Signal => signal_child(pid, &payload)?,
                _ => bail!("invalid guest-bound stream frame {kind:?}"),
            }
        } else if descriptors[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            // SAFETY: negative pid targets the child process group.
            unsafe { libc::kill(-pid, libc::SIGHUP) };
            let _ = child.wait();
            bail!("interactive host connection closed");
        }
    }
}

fn drain_pty(master: &mut fs::File, stream: &mut VsockStream, buffer: &mut [u8]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        if Instant::now() >= deadline {
            return Ok(());
        }
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // A zero-timeout poll drains bytes already committed by the exited
        // shell without waiting indefinitely for a noisy disowned descendant.
        // SAFETY: descriptor points to one initialized pollfd.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready <= 0 || descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            return Ok(());
        }
        match master.read(buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => write_stream_frame(&mut *stream, StreamKind::Stdout, &buffer[..count])?,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
            Err(error) => return Err(error).context("drain PTY output"),
        }
    }
}

fn prepare(
    command: &mut Command,
    user: &User,
    session_dir: &Path,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.uid(user.uid).gid(user.gid);
    // Give each captured operation its own process group so helpers which
    // outlive the direct child can be stopped without touching PID 1 or an
    // unrelated guest command.
    // SAFETY: setsid is async-signal-safe and allocates no Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    populate_environment(command, user, session_dir, env)
}

fn find_git() -> Result<&'static Path> {
    ["/usr/bin/git", "/usr/local/bin/git"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .context(
            "Git is not installed in this OCI environment; publish or choose an environment containing git",
        )
}

fn populate_environment(
    command: &mut Command,
    user: &User,
    session_dir: &Path,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    crate::session::apply_environment(command, session_dir)?;
    command
        .env("HOME", &user.home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("SHELL", &user.shell)
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    for (name, value) in env {
        command.env(name, value);
    }
    Ok(())
}

struct Captured {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl Captured {
    fn into_response(self) -> Response {
        Response::CommandResult {
            exit_code: self.exit_code,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

fn run_captured(mut command: Command) -> Result<Captured> {
    let (mut child, _tracking) =
        crate::reaper::spawn(&mut command).context("execute guest command")?;
    let pid = child.id() as libc::pid_t;
    let stdout = child
        .stdout
        .take()
        .context("command stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("command stderr was not piped")?;
    let done = Arc::new(AtomicBool::new(false));
    let stdout_done = Arc::clone(&done);
    let stderr_done = Arc::clone(&done);
    let stdout = thread::spawn(move || read_bounded(stdout, stdout_done));
    let stderr = thread::spawn(move || read_bounded(stderr, stderr_done));
    child_exit_ready(pid, false).context("wait for guest command")?;
    // SAFETY: the direct child created a distinct session/process group.
    // The child has not been reaped, so its process-group ID cannot be reused.
    unsafe { libc::kill(-pid, libc::SIGHUP) };
    let status = child.wait().context("reap guest command")?;
    done.store(true, Ordering::Release);
    let stdout = stdout
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    ensure!(
        !stdout.exceeded && !stderr.exceeded,
        "command output exceeded {} bytes per stream",
        MAX_CAPTURED_OUTPUT
    );
    Ok(Captured {
        exit_code: exit_code(status),
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
    })
}

struct BoundedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

fn read_bounded(mut input: impl Read + AsRawFd, done: Arc<AtomicBool>) -> Result<BoundedRead> {
    set_nonblocking(input.as_raw_fd())?;
    let mut bytes = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0_u8; 16 * 1024];
    let mut finish_by = None;
    loop {
        if done.load(Ordering::Acquire) && finish_by.is_none() {
            finish_by = Some(Instant::now() + Duration::from_millis(100));
        }
        match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len() < MAX_CAPTURED_OUTPUT {
                    let retained = count.min(MAX_CAPTURED_OUTPUT - bytes.len());
                    bytes.extend_from_slice(&buffer[..retained]);
                    exceeded |= retained != count;
                } else {
                    exceeded = true;
                }
                if finish_by.is_some_and(|deadline| Instant::now() >= deadline) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if finish_by.is_some() {
                    break;
                }
                poll_readable(input.as_raw_fd(), 100)?;
            }
            Err(error) => return Err(error).context("read command output"),
        }
    }
    Ok(BoundedRead { bytes, exceeded })
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: F_GETFL/F_SETFL operate on a valid live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("make command pipe nonblocking");
    }
    Ok(())
}

fn poll_readable(fd: RawFd, timeout_ms: libc::c_int) -> Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor points to one initialized pollfd.
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("poll command output");
        }
    }
    Ok(())
}

fn child_exit_ready(pid: libc::pid_t, nonblocking: bool) -> Result<bool> {
    loop {
        // WNOWAIT observes termination without reaping. This keeps the PID and
        // process-group identity reserved until callers have signalled any
        // surviving descendants and then call Child::wait.
        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let mut options = libc::WEXITED | libc::WNOWAIT;
        if nonblocking {
            options |= libc::WNOHANG;
        }
        // SAFETY: information points to writable storage; pid is the live
        // direct child created by this handler.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                information.as_mut_ptr(),
                options,
            )
        };
        if result == 0 {
            // SAFETY: successful waitid initialized siginfo_t. si_pid is zero
            // only for WNOHANG when no child has changed state.
            return Ok(unsafe { information.assume_init().si_pid() } != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("observe guest child status");
        }
    }
}

fn validate_argv(argv: &[String]) -> Result<()> {
    ensure!(!argv.is_empty(), "argv must contain an executable");
    ensure!(argv.len() <= MAX_ARGV_ITEMS, "argv has too many items");
    ensure!(
        argv.iter().map(String::len).sum::<usize>() <= MAX_ARG_BYTES,
        "argv exceeds {MAX_ARG_BYTES} bytes"
    );
    for value in argv {
        validate_text("argument", value, MAX_ARG_BYTES)?;
    }
    Ok(())
}

fn validate_environment(env: &BTreeMap<String, String>) -> Result<()> {
    ensure!(env.len() <= MAX_ENV_ITEMS, "too many environment variables");
    for (name, value) in env {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                && !name.as_bytes()[0].is_ascii_digit(),
            "invalid environment variable name {name:?}"
        );
        validate_text("environment value", value, MAX_ARG_BYTES)?;
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, maximum: usize) -> Result<()> {
    ensure!(!value.is_empty(), "{label} must not be empty");
    ensure!(value.len() <= maximum, "{label} is too large");
    ensure!(!value.contains('\0'), "{label} contains NUL");
    Ok(())
}

fn contained_path(workspace: &Path, requested: &str, must_exist: bool) -> Result<PathBuf> {
    let requested = Path::new(requested);
    ensure!(requested.is_absolute(), "workspace path must be absolute");
    let relative = requested.strip_prefix(workspace).with_context(|| {
        format!(
            "path {} is outside {}",
            requested.display(),
            workspace.display()
        )
    })?;
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "workspace path contains non-normal components"
    );
    let result = workspace.join(relative);
    let check = if must_exist {
        result.as_path()
    } else {
        result.parent().unwrap_or(workspace)
    };
    ensure_contained_existing(workspace, check)?;
    if must_exist {
        ensure!(
            result.exists(),
            "workspace path {} does not exist",
            result.display()
        );
    }
    Ok(result)
}

fn ensure_contained_existing(workspace: &Path, path: &Path) -> Result<()> {
    let workspace = fs::canonicalize(workspace)?;
    let path = fs::canonicalize(path)
        .with_context(|| format!("canonicalize workspace path {}", path.display()))?;
    ensure!(
        path.starts_with(&workspace),
        "workspace path escapes through a symlink"
    );
    Ok(())
}

fn resize_pty(fd: RawFd, payload: &[u8]) -> Result<()> {
    ensure!(payload.len() == 4, "resize payload must be four bytes");
    let size = libc::winsize {
        ws_row: u16::from_be_bytes([payload[0], payload[1]]).max(1),
        ws_col: u16::from_be_bytes([payload[2], payload[3]]).max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is a valid PTY master and size points to initialized storage.
    ensure!(
        unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) } == 0,
        "resize PTY failed"
    );
    Ok(())
}

fn signal_child(pid: libc::pid_t, payload: &[u8]) -> Result<()> {
    ensure!(payload.len() == 4, "signal payload must be four bytes");
    let signal = i32::from_be_bytes(payload.try_into().unwrap());
    ensure!(
        matches!(
            signal,
            libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
        ),
        "signal is not allowed"
    );
    // SAFETY: negative pid targets only the child process group.
    ensure!(
        unsafe { libc::kill(-pid, signal) } == 0,
        "send signal failed"
    );
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_environment_names() {
        assert!(validate_environment(&BTreeMap::from([("SAFE_2".into(), "value".into())])).is_ok());
        assert!(
            validate_environment(&BTreeMap::from([("BAD=NAME".into(), "value".into())])).is_err()
        );
        assert!(validate_environment(&BTreeMap::from([("2BAD".into(), "value".into())])).is_err());
    }

    #[test]
    fn path_validation_refuses_traversal() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            contained_path(
                workspace.path(),
                &format!("{}/repo", workspace.path().display()),
                false
            )
            .is_ok()
        );
        assert!(
            contained_path(
                workspace.path(),
                &format!("{}/../escape", workspace.path().display()),
                false
            )
            .is_err()
        );
        assert!(contained_path(workspace.path(), "/etc", true).is_err());
    }

    #[test]
    fn captured_command_does_not_wait_for_inherited_pipe() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30 & printf inherited-pipe-ok"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Match prepare(): the background process must be isolated from the
        // test runner and safely targetable as one command process group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let result = run_captured(command).unwrap();
        assert_eq!(result.stdout, "inherited-pipe-ok");
        assert_eq!(result.exit_code, 0);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
