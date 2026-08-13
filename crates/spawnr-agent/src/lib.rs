//! Minimal, private guest control plane for Spawnr development computers.
//!
//! The agent listens only on AF_VSOCK. Every connection carries one framed
//! request, which makes slow or interactive operations independent: an open
//! shell cannot block health checks or graceful shutdown requests.

mod account;
mod agent_proxy;
mod command;
mod mount;
mod reaper;
mod session;

use anyhow::{Context, Result, bail};
use spawnr_protocol::{PROTOCOL_VERSION, Request, Response, read_json, write_json};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use vsock::{VMADDR_CID_ANY, VsockListener, VsockStream};

#[derive(Debug, Clone)]
pub struct Config {
    pub machine_name: String,
    pub control_port: u32,
    pub dev_user: String,
    pub workspace: PathBuf,
    pub session_dir: PathBuf,
}

/// Minimal PID 1 preparation for OCI root filesystems that do not ship an
/// init system. The initramfs already mounted the environment root and moved
/// dev/proc/sys into it; here we finish ephemeral state and networking.
pub fn initialize_pid_one() -> Result<()> {
    require_root()?;
    redirect_standard_error_to_console().context("connect PID 1 diagnostics to /dev/console")?;
    for (source, target, kind, flags, data) in [
        (
            Some("proc"),
            "/proc",
            Some("proc"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            None,
        ),
        (
            Some("sysfs"),
            "/sys",
            Some("sysfs"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            None,
        ),
        (
            Some("devtmpfs"),
            "/dev",
            Some("devtmpfs"),
            libc::MS_NOSUID,
            Some("mode=0755"),
        ),
        (
            Some("tmpfs"),
            "/run",
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755,size=32m"),
        ),
    ] {
        std::fs::create_dir_all(target)?;
        if !is_mountpoint(target)? {
            mount_named(source, target, kind, flags, data)?;
        }
    }
    std::fs::create_dir_all("/dev/pts")?;
    if !is_mountpoint("/dev/pts")? {
        mount_named(
            Some("devpts"),
            "/dev/pts",
            Some("devpts"),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            Some("mode=0620,ptmxmode=0666"),
        )?;
    }
    Ok(())
}

/// Configure the virtio network after session tmpfs exists, so DHCP-derived
/// resolver state is ephemeral rather than a mutation of the OCI environment.
pub fn configure_pid_one_network() -> Result<()> {
    require_root()?;
    configure_network()
}

pub fn start_pid_one_reaper() -> Result<()> {
    require_root()?;
    reaper::start()
}

fn redirect_standard_error_to_console() -> Result<()> {
    use std::os::fd::AsRawFd;
    let console = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/console")
        .context("open guest console")?;
    // SAFETY: dup2 copies the live console descriptor onto stderr. Rust's
    // process-global stderr handle remains descriptor 2 and needs no rebuild.
    if unsafe { libc::dup2(console.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error()).context("redirect stderr to guest console");
    }
    Ok(())
}

fn is_mountpoint(target: &str) -> Result<bool> {
    let target = std::fs::canonicalize(target)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    Ok(mountinfo.lines().any(|line| {
        line.split_ascii_whitespace()
            .nth(4)
            .is_some_and(|path| std::path::Path::new(path) == target)
    }))
}

fn mount_named(
    source: Option<&str>,
    target: &str,
    kind: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    use std::ffi::CString;
    let source = source.map(CString::new).transpose()?;
    let target = CString::new(target)?;
    let kind = kind.map(CString::new).transpose()?;
    let data = data.map(CString::new).transpose()?;
    // SAFETY: all optional pointers reference live, NUL-terminated strings.
    let status = unsafe {
        libc::mount(
            source.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            target.as_ptr(),
            kind.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |v| v.as_ptr().cast()),
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("mount guest pseudo-filesystem");
    }
    Ok(())
}

fn configure_network() -> Result<()> {
    use std::process::Command;
    let busybox = std::path::Path::new("/usr/libexec/spawnr-busybox");
    if !busybox.is_file() {
        bail!("guest integration is incomplete: {busybox:?} is missing");
    }
    let status = Command::new(busybox)
        .args(["ip", "link", "set", "lo", "up"])
        .status()
        .context("bring guest loopback up")?;
    if !status.success() {
        bail!("failed to bring guest loopback up");
    }
    let interface = std::fs::read_dir("/sys/class/net")?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find(|name| name != "lo")
        .context("guest virtio network interface did not appear")?;
    let interface = interface
        .to_str()
        .context("network interface is not UTF-8")?;
    let status = Command::new(busybox)
        .args(["ip", "link", "set", interface, "up"])
        .status()
        .with_context(|| format!("bring guest interface {interface} up"))?;
    if !status.success() {
        bail!("failed to bring guest interface {interface} up");
    }
    let status = Command::new(busybox)
        .args([
            "udhcpc",
            "-q",
            "-f",
            "-i",
            interface,
            "-s",
            "/usr/libexec/spawnr-udhcpc",
        ])
        .status()
        .context("configure guest network with DHCP")?;
    if !status.success() {
        bail!("DHCP configuration failed for {interface}");
    }
    Ok(())
}

#[derive(Debug)]
struct State {
    config: Config,
    user: account::User,
}

pub fn initialize(config: &Config) -> Result<()> {
    require_root()?;
    account::ensure_development_user(&config.dev_user)?;
    mount::ensure_workspace_mounted(&config.workspace)?;
    let user = account::lookup(&config.dev_user)?;
    account::chown(&config.workspace, &user)?;
    mount::ensure_session_tmpfs(&config.session_dir)?;
    account::chown(&config.session_dir, &user)?;
    Ok(())
}

pub fn serve(config: Config) -> Result<()> {
    require_root()?;
    let user = account::lookup(&config.dev_user).with_context(|| {
        format!(
            "development account {:?} is missing; run spawnr-agent --initialize-workspace",
            config.dev_user
        )
    })?;
    mount::assert_workspace_is_separate(&config.workspace)?;
    mount::assert_session_is_tmpfs(&config.session_dir)?;

    let listener = VsockListener::bind_with_cid_port(VMADDR_CID_ANY, config.control_port)
        .with_context(|| format!("listen on AF_VSOCK port {}", config.control_port))?;
    let state = Arc::new(State { config, user });

    loop {
        let (stream, peer) = listener.accept().context("accept AF_VSOCK connection")?;
        let state = Arc::clone(&state);
        thread::Builder::new()
            .name(format!("spawnr-vsock-{}", peer.port()))
            .spawn(move || {
                if let Err(error) = handle_connection(stream, &state) {
                    eprintln!("spawnr-agent: connection failed: {error:#}");
                }
            })
            .context("spawn connection handler")?;
    }
}

fn handle_connection(mut stream: VsockStream, state: &State) -> Result<()> {
    let request: Request = read_json(&mut stream).context("read guest request")?;
    let streaming = matches!(&request, Request::Exec { tty: true, .. });
    let result = match request {
        Request::Health => write_json(
            &mut stream,
            &Response::Health {
                protocol: PROTOCOL_VERSION,
                machine_name: state.config.machine_name.clone(),
                workspace_mounted: mount::workspace_is_separate(&state.config.workspace),
            },
        ),
        Request::ConfigureSession(config) => {
            session::configure(&state.config.session_dir, &state.user, config)
                .and_then(|message| write_json(&mut stream, &Response::Ok { message }))
        }
        Request::CloneRepository {
            repository,
            destination,
        } => command::clone_repository(
            &state.config.workspace,
            &state.config.session_dir,
            &state.user,
            &repository,
            &destination,
        )
        .and_then(|response| write_json(&mut stream, &response)),
        Request::Exec {
            argv,
            cwd,
            env,
            tty,
            rows,
            cols,
        } => command::exec(
            &mut stream,
            &state.config.workspace,
            &state.config.session_dir,
            &state.user,
            argv,
            cwd,
            env,
            tty,
            rows,
            cols,
        ),
        Request::WorkspaceStatus { repository_path } => command::workspace_status(
            &state.config.workspace,
            &state.config.session_dir,
            &state.user,
            &repository_path,
        )
        .and_then(|response| write_json(&mut stream, &response)),
        Request::Shutdown => {
            write_json(
                &mut stream,
                &Response::Ok {
                    message: "shutdown requested".into(),
                },
            )?;
            command::request_shutdown()
        }
    };

    if let Err(error) = result {
        if !streaming {
            let _ = write_json(
                &mut stream,
                &Response::Error {
                    message: format!("{error:#}"),
                },
            );
        }
        return Err(error);
    }
    Ok(())
}

fn require_root() -> Result<()> {
    // SAFETY: geteuid has no preconditions or side effects.
    if unsafe { libc::geteuid() } != 0 {
        bail!("spawnr-agent must run as root so it can mount storage and enter the dev account");
    }
    Ok(())
}

fn write_private(path: &std::path::Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("session file has no parent")?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("install {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn protocol_ports_are_distinct() {
        assert_ne!(
            spawnr_protocol::CONTROL_PORT,
            spawnr_protocol::SSH_AGENT_PORT
        );
    }
}
