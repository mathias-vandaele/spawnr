//! Cloud Hypervisor lifecycle and host capability diagnostics.

use crate::credentials::{HostCredentials, SSH_AGENT_VSOCK_PORT};
use crate::paths::Paths;
use crate::process::{
    self, ManagedProcessStatus, SpawnedProcess, inspect_pid_file, remove_pid_file,
    remove_stale_pid_file, spawn_managed, terminate, terminate_spawned, wait_for_exit,
};
use crate::state::MachineRecord;
use crate::storage::{MachinePaths, validate_ext4_image};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const KVM_GET_API_VERSION: libc::Ioctl = 0xAE00;
const EXPECTED_KVM_API_VERSION: libc::c_int = 12;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(15);
const FORCE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmmStatus {
    Stopped,
    Running,
    Degraded,
}

#[derive(Debug, Clone)]
pub struct Vmm {
    cloud_hypervisor: PathBuf,
    passt: PathBuf,
    kernel: PathBuf,
    initramfs: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub remedy: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DoctorReport {
    pub ready: bool,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn collect(paths: Option<&Paths>) -> Self {
        let mut checks = vec![architecture_check(), kvm_check()];
        let bundled = |name: &str| {
            paths
                .map(|paths| paths.bin_dir().join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        };
        checks.push(tool_check(
            "cloud-hypervisor",
            "SPAWNR_CLOUD_HYPERVISOR",
            &bundled("cloud-hypervisor"),
            "cloud-hypervisor",
            "install the pinned Cloud Hypervisor release or set SPAWNR_CLOUD_HYPERVISOR",
        ));
        checks.push(tool_check(
            "passt",
            "SPAWNR_PASST",
            &bundled("passt"),
            "passt",
            "install passt (vhost-user networking) or set SPAWNR_PASST",
        ));
        checks.push(tool_check(
            "mkfs.ext4",
            "SPAWNR_MKFS_EXT4",
            Path::new("/usr/bin/mkfs.ext4"),
            "mkfs.ext4",
            "install e2fsprogs or set SPAWNR_MKFS_EXT4",
        ));
        checks.push(asset_check(
            "guest kernel",
            "SPAWNR_KERNEL",
            &bundled("vmlinux"),
            true,
            "install Spawnr guest boot assets or set SPAWNR_KERNEL",
        ));
        checks.push(asset_check(
            "guest initramfs",
            "SPAWNR_INITRAMFS",
            &bundled("initramfs"),
            true,
            "install the Spawnr initramfs or set SPAWNR_INITRAMFS",
        ));
        Self {
            ready: checks.iter().all(|check| check.ok),
            checks,
        }
    }

    pub fn render_text(&self) -> String {
        let mut output = String::new();
        for check in &self.checks {
            let mark = if check.ok { "✓" } else { "✗" };
            output.push_str(&format!("{mark} {:<20} {}\n", check.name, check.detail));
            if !check.ok
                && let Some(remedy) = &check.remedy
            {
                output.push_str(&format!("  fix: {remedy}\n"));
            }
        }
        output.push_str(if self.ready {
            "\nSpawnr can start KVM development computers.\n"
        } else {
            "\nSpawnr cannot start a development computer until the failed checks are fixed.\n"
        });
        output
    }
}

impl Vmm {
    pub fn discover(paths: &Paths) -> Result<Self> {
        ensure!(
            env::consts::OS == "linux" && env::consts::ARCH == "x86_64",
            "Spawnr V1 requires Linux x86_64"
        );
        verify_kvm()?;
        let cloud_hypervisor = process::resolve_executable(
            "SPAWNR_CLOUD_HYPERVISOR",
            &paths.bin_dir().join("cloud-hypervisor"),
            "cloud-hypervisor",
        )?;
        let passt =
            process::resolve_executable("SPAWNR_PASST", &paths.bin_dir().join("passt"), "passt")?;
        let kernel = resolve_asset("SPAWNR_KERNEL", &paths.bin_dir().join("vmlinux"))?
            .context("Spawnr guest kernel is unavailable")?;
        let initramfs = Some(
            resolve_asset("SPAWNR_INITRAMFS", &paths.bin_dir().join("initramfs"))?
                .context("Spawnr guest initramfs is unavailable")?,
        );
        Ok(Self {
            cloud_hypervisor,
            passt,
            kernel,
            initramfs,
        })
    }

    pub fn with_components(
        cloud_hypervisor: impl Into<PathBuf>,
        passt: impl Into<PathBuf>,
        kernel: impl Into<PathBuf>,
        initramfs: Option<PathBuf>,
    ) -> Self {
        Self {
            cloud_hypervisor: cloud_hypervisor.into(),
            passt: passt.into(),
            kernel: kernel.into(),
            initramfs,
        }
    }

    pub fn status(&self, paths: &MachinePaths) -> Result<VmmStatus> {
        let vmm = inspect_pid_file(&paths.vmm_pid_file)?.is_running();
        let network = inspect_pid_file(&paths.network_pid_file)?.is_running();
        Ok(match (vmm, network) {
            (true, true) => VmmStatus::Running,
            (false, false) => VmmStatus::Stopped,
            _ => VmmStatus::Degraded,
        })
    }

    pub fn command(
        &self,
        paths: &MachinePaths,
        record: &MachineRecord,
        verbose: u8,
    ) -> Result<Command> {
        self.command_with_agent(paths, record, verbose, None)
    }

    fn command_with_agent(
        &self,
        paths: &MachinePaths,
        record: &MachineRecord,
        verbose: u8,
        ssh_agent: Option<&Path>,
    ) -> Result<Command> {
        validate_machine_hostname(&record.name)?;
        let mac_address = validate_mac_address(&record.mac_address)?;
        validate_ext4_image(&paths.environment_disk)?;
        validate_ext4_image(&paths.workspace_disk)?;
        for socket in [
            &paths.api_socket,
            &paths.vsock_socket,
            &paths.network_socket,
        ] {
            validate_unix_socket_path(socket)?;
        }
        let mut agent_endpoint = paths.vsock_socket.as_os_str().to_os_string();
        agent_endpoint.push(format!("_{SSH_AGENT_VSOCK_PORT}"));
        validate_unix_socket_path(&PathBuf::from(agent_endpoint))
            .context("SSH-agent vsock endpoint would exceed the Unix socket path limit")?;
        for path in [
            &paths.environment_disk,
            &paths.workspace_disk,
            &paths.api_socket,
            &paths.vsock_socket,
            &paths.network_socket,
            &paths.serial_log,
            &paths.root,
        ] {
            option_path(path)?;
        }

        let mut command = Command::new(&self.cloud_hypervisor);
        command
            .arg("--kernel")
            .arg(&self.kernel)
            .arg("--cmdline")
            // systemd uses this as the guest hostname. The installed agent
            // service passes `%H` to `--machine-name`, so a cached OCI base
            // remains machine-neutral and needs no per-instance rewrite.
            .arg(format!(
                "console=hvc0 root=/dev/vda rw rootfstype=ext4 rootwait panic=1 init=/usr/libexec/spawnr-agent spawnr.machine_name={}",
                record.name,
            ))
            .arg("--cpus")
            .arg("boot=4")
            .arg("--memory")
            .arg("size=4G,shared=on")
            .arg("--disk")
            .arg(format!(
                "path={},id=environment,image_type=raw,direct=off,sparse=on,serial=SPAWNR_ENV",
                option_path(&paths.environment_disk)?
            ))
            .arg(format!(
                "path={},id=workspace,image_type=raw,direct=off,sparse=on,serial=SPAWNR_WORKSPACE",
                option_path(&paths.workspace_disk)?
            ))
            .arg("--net")
            .arg(format!(
                "vhost_user=on,vhost_mode=client,socket={},mac={},id=network",
                option_path(&paths.network_socket)?,
                mac_address
            ))
            .arg("--vsock")
            .arg(format!(
                "cid={},socket={},id=control",
                record.vsock_cid,
                option_path(&paths.vsock_socket)?
            ))
            .arg("--api-socket")
            .arg(format!("path={}", option_path(&paths.api_socket)?))
            .arg("--console")
            .arg(format!("file={}", option_path(&paths.serial_log)?))
            .arg("--serial")
            .arg("null")
            .arg("--rng")
            .arg("src=/dev/urandom")
            .arg("--landlock")
            .arg("--landlock-rules")
            .arg(format!("path={},access=rw", option_path(&paths.root)?));
        if let Some(agent) = ssh_agent {
            command.arg(format!("path={},access=rw", option_path(agent)?));
        }
        if let Some(initramfs) = &self.initramfs {
            command.arg("--initramfs").arg(initramfs);
        }
        for _ in 0..verbose.min(3) {
            command.arg("-v");
        }
        Ok(command)
    }

    fn network_command(&self, paths: &MachinePaths, verbose: u8) -> Result<Command> {
        let host_dns = host_ipv4_resolver()?;
        let mut command = Command::new(&self.passt);
        // Passt is outbound-only for Spawnr V1. Explicitly disable host-port
        // forwarding even though current passt releases default to `none`, and
        // do not give the guest passt's special gateway alias for host-local
        // services. Host firewall policy still governs services deliberately
        // bound to an externally reachable host address.
        command
            .arg("--vhost-user")
            .arg("--socket")
            .arg(&paths.network_socket)
            .arg("--foreground")
            // Exit when the sole Cloud Hypervisor vhost-user client goes
            // away, preventing a VMM crash from orphaning passt.
            .arg("--one-off")
            .arg("--no-map-gw")
            // The host commonly uses a loopback stub resolver, which cannot
            // be handed directly to a guest. Passt intercepts this otherwise
            // unrouted link-local address and forwards only DNS traffic to
            // the host's configured resolver.
            .arg("--dns")
            .arg("169.254.0.53")
            .arg("--dns-forward")
            .arg("169.254.0.53")
            .arg("--dns-host")
            .arg(host_dns.to_string())
            .arg("--tcp-ports")
            .arg("none")
            .arg("--udp-ports")
            .arg("none");
        if verbose == 0 {
            command.arg("--quiet");
        } else {
            command.arg("--debug");
        }
        Ok(command)
    }

    pub fn start(
        &self,
        paths: &MachinePaths,
        record: &MachineRecord,
        credentials: &HostCredentials,
        verbose: u8,
    ) -> Result<()> {
        paths.assert_domain_layout(record)?;
        let vmm_status = inspect_pid_file(&paths.vmm_pid_file)?;
        let network_status = inspect_pid_file(&paths.network_pid_file)?;
        if vmm_status.is_running() && network_status.is_running() {
            return Ok(());
        }
        if vmm_status.is_running() {
            self.stop(paths, record, verbose)
                .context("repair VM whose network helper is not running")?;
        } else {
            match vmm_status {
                ManagedProcessStatus::Stale { .. } => {
                    remove_stale_pid_file(&paths.vmm_pid_file)?;
                }
                ManagedProcessStatus::Missing | ManagedProcessStatus::Running(_) => {}
            }
            self.stop_network(paths)?;
        }
        paths.clear_session(record)?;
        let mut command =
            self.command_with_agent(paths, record, verbose, credentials.ssh_auth_sock())?;

        let mut network = self.network_command(paths, verbose)?;
        let mut network_process =
            spawn_managed(&mut network, &paths.network_pid_file, &paths.vmm_log)
                .context("start passt networking")?;
        if let Err(error) = wait_for_socket(
            &paths.network_socket,
            &mut network_process.child,
            START_TIMEOUT,
        ) {
            let cleanup = self.cleanup_failed_start(paths, record, None, &mut network_process);
            return Err(with_cleanup(
                error.context("start outbound VM networking"),
                cleanup,
            ));
        }

        match credentials.expose_ssh_agent(&paths.vsock_socket) {
            Ok(Some(link)) => link.persist(),
            Ok(None) => {}
            Err(error) => {
                let cleanup = self.cleanup_failed_start(paths, record, None, &mut network_process);
                return Err(with_cleanup(
                    error.context("expose host SSH-agent capability"),
                    cleanup,
                ));
            }
        }
        let mut vmm = match spawn_managed(&mut command, &paths.vmm_pid_file, &paths.vmm_log) {
            Ok(vmm) => vmm,
            Err(error) => {
                let cleanup = self.cleanup_failed_start(paths, record, None, &mut network_process);
                return Err(with_cleanup(
                    error.context("start Cloud Hypervisor"),
                    cleanup,
                ));
            }
        };
        if let Err(error) = wait_for_socket(&paths.api_socket, &mut vmm.child, START_TIMEOUT) {
            let failure = error.context(format!(
                "Cloud Hypervisor failed to start; inspect {}",
                paths.vmm_log.display()
            ));
            let cleanup =
                self.cleanup_failed_start(paths, record, Some(&mut vmm), &mut network_process);
            return Err(with_cleanup(failure, cleanup));
        }
        Ok(())
    }

    fn cleanup_failed_start(
        &self,
        paths: &MachinePaths,
        record: &MachineRecord,
        vmm: Option<&mut SpawnedProcess>,
        network: &mut SpawnedProcess,
    ) -> Result<()> {
        let mut first_error = None;
        if let Some(vmm) = vmm
            && let Err(error) = terminate_spawned(vmm, FORCE_TIMEOUT)
                .and_then(|()| remove_pid_file(&paths.vmm_pid_file, &vmm.identity))
        {
            first_error = Some(error.context("clean failed Cloud Hypervisor start"));
        }
        if let Err(error) = terminate_spawned(network, FORCE_TIMEOUT)
            .and_then(|()| remove_pid_file(&paths.network_pid_file, &network.identity))
        {
            first_error.get_or_insert(error.context("clean failed passt start"));
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        paths.clear_session(record)
    }

    pub fn stop(&self, paths: &MachinePaths, record: &MachineRecord, _verbose: u8) -> Result<()> {
        paths.assert_owned(record)?;
        let mut first_error = None;
        match inspect_pid_file(&paths.vmm_pid_file)? {
            ManagedProcessStatus::Missing => {}
            ManagedProcessStatus::Stale { recorded, .. } => {
                remove_pid_file(&paths.vmm_pid_file, &recorded)?;
            }
            ManagedProcessStatus::Running(identity) => {
                // The guest agent normally shuts down first. If unavailable,
                // ask the virtual power button before forcing the VMM down.
                let _ = cloud_hypervisor_request(&paths.api_socket, "vm.power-button");
                let exited = wait_for_exit(&identity, GRACEFUL_TIMEOUT)?;
                if !exited {
                    let _ = cloud_hypervisor_request(&paths.api_socket, "vm.shutdown");
                }
                if let Err(error) = if wait_for_exit(&identity, FORCE_TIMEOUT)? {
                    Ok(())
                } else {
                    terminate(&identity, FORCE_TIMEOUT)
                } {
                    first_error = Some(error.context("stop Cloud Hypervisor"));
                }
                if !identity.is_current()? {
                    remove_pid_file(&paths.vmm_pid_file, &identity)?;
                }
            }
        }
        if let Err(error) = self.stop_network(paths) {
            first_error.get_or_insert(error);
        }
        if first_error.is_none() {
            paths.clear_session(record)?;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    fn stop_network(&self, paths: &MachinePaths) -> Result<()> {
        match inspect_pid_file(&paths.network_pid_file)? {
            ManagedProcessStatus::Missing => Ok(()),
            ManagedProcessStatus::Stale { recorded, .. } => {
                remove_pid_file(&paths.network_pid_file, &recorded)
            }
            ManagedProcessStatus::Running(identity) => {
                terminate(&identity, FORCE_TIMEOUT)?;
                remove_pid_file(&paths.network_pid_file, &identity)
            }
        }
    }
}

pub fn is_running(paths: &Paths, record: &MachineRecord) -> Result<bool> {
    Ok(status(paths, record)? == VmmStatus::Running)
}

pub fn status(paths: &Paths, record: &MachineRecord) -> Result<VmmStatus> {
    let machine = MachinePaths::for_record(paths, record);
    Vmm::with_components("", "", "", None).status(&machine)
}

pub fn start(
    paths: &Paths,
    record: &MachineRecord,
    credentials: &HostCredentials,
    verbose: u8,
) -> Result<()> {
    Vmm::discover(paths)?.start(
        &MachinePaths::for_record(paths, record),
        record,
        credentials,
        verbose,
    )
}

pub fn stop(paths: &Paths, record: &MachineRecord, verbose: u8) -> Result<()> {
    // Stop does not depend on boot assets still being installed.
    Vmm::with_components("", "", "", None).stop(
        &MachinePaths::for_record(paths, record),
        record,
        verbose,
    )
}

pub fn doctor(paths: &Paths, json: bool) -> Result<()> {
    let report = DoctorReport::collect(Some(paths));
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render_text());
    }
    ensure!(
        report.ready,
        "host requirements are incomplete; see the failed checks above"
    );
    Ok(())
}

fn with_cleanup(primary: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => primary.context(format!(
            "automatic cleanup also failed; ownership records were preserved: {cleanup:#}"
        )),
    }
}

fn architecture_check() -> DoctorCheck {
    let found = format!("{} {}", env::consts::OS, env::consts::ARCH);
    let ok = env::consts::OS == "linux" && env::consts::ARCH == "x86_64";
    DoctorCheck {
        name: "platform".into(),
        ok,
        detail: found,
        remedy: (!ok).then(|| "Spawnr V1 requires Linux on x86_64".into()),
    }
}

fn kvm_check() -> DoctorCheck {
    match verify_kvm() {
        Ok(()) => DoctorCheck {
            name: "KVM".into(),
            ok: true,
            detail: "/dev/kvm is accessible (API 12)".into(),
            remedy: None,
        },
        Err(error) => DoctorCheck {
            name: "KVM".into(),
            ok: false,
            detail: format!("{error:#}"),
            remedy: Some(
                "inspect /dev/kvm, enable virtualization in firmware, and grant user access".into(),
            ),
        },
    }
}

fn host_ipv4_resolver() -> Result<Ipv4Addr> {
    let contents = fs::read_to_string("/etc/resolv.conf")
        .context("read host /etc/resolv.conf for passt DNS forwarding")?;
    parse_ipv4_resolver(&contents)
}

fn parse_ipv4_resolver(contents: &str) -> Result<Ipv4Addr> {
    for line in contents.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        if let Some(address) = fields.next()
            && let Ok(IpAddr::V4(address)) = address.parse()
        {
            return Ok(address);
        }
    }
    bail!(
        "host /etc/resolv.conf has no IPv4 nameserver; configure one so passt can provide guest DNS"
    )
}

fn tool_check(
    name: &str,
    variable: &str,
    bundled: &Path,
    command: &str,
    remedy: &str,
) -> DoctorCheck {
    match process::resolve_executable(variable, bundled, command) {
        Ok(path) => DoctorCheck {
            name: name.into(),
            ok: true,
            detail: path.display().to_string(),
            remedy: None,
        },
        Err(error) => DoctorCheck {
            name: name.into(),
            ok: false,
            detail: error.to_string(),
            remedy: Some(remedy.into()),
        },
    }
}

fn asset_check(
    name: &str,
    variable: &str,
    bundled: &Path,
    required: bool,
    remedy: &str,
) -> DoctorCheck {
    match resolve_asset(variable, bundled) {
        Ok(Some(path)) => DoctorCheck {
            name: name.into(),
            ok: true,
            detail: path.display().to_string(),
            remedy: None,
        },
        Ok(None) if !required => DoctorCheck {
            name: name.into(),
            ok: true,
            detail: "not installed (optional)".into(),
            remedy: None,
        },
        Ok(None) => DoctorCheck {
            name: name.into(),
            ok: false,
            detail: "not found".into(),
            remedy: Some(remedy.into()),
        },
        Err(error) => DoctorCheck {
            name: name.into(),
            ok: false,
            detail: error.to_string(),
            remedy: Some(remedy.into()),
        },
    }
}

fn verify_kvm() -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/dev/kvm")
        .context("KVM is unavailable because /dev/kvm is not accessible")?;
    ensure!(
        file.metadata()?.file_type().is_char_device(),
        "/dev/kvm is not a character device"
    );
    let version = unsafe { libc::ioctl(file.as_raw_fd(), KVM_GET_API_VERSION, 0) };
    if version < 0 {
        return Err(io::Error::last_os_error()).context("query KVM API version");
    }
    ensure!(
        version == EXPECTED_KVM_API_VERSION,
        "unexpected KVM API version {version} (expected {EXPECTED_KVM_API_VERSION})"
    );
    Ok(())
}

fn resolve_asset(variable: &str, bundled: &Path) -> Result<Option<PathBuf>> {
    let chosen = env::var_os(variable).map(PathBuf::from).or_else(|| {
        bundled
            .try_exists()
            .ok()
            .filter(|exists| *exists)
            .map(|_| bundled.to_owned())
    });
    let Some(path) = chosen else {
        return Ok(None);
    };
    let file = File::open(&path).with_context(|| format!("open asset {}", path.display()))?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(Some(fs::canonicalize(&path).with_context(|| {
        format!("canonicalize {}", path.display())
    })?))
}

fn wait_for_socket(path: &Path, child: &mut std::process::Child, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("process exited before becoming ready ({status})");
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => return Ok(()),
            Ok(_) => bail!("runtime endpoint is not a socket: {}", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect runtime socket"),
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {}", path.display());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn cloud_hypervisor_request(socket: &Path, endpoint: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to Cloud Hypervisor at {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "PUT /api/v1/{endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .context("Cloud Hypervisor returned an invalid HTTP status")?;
    ensure!(
        (200..300).contains(&code),
        "Cloud Hypervisor {endpoint} failed with HTTP {code}"
    );
    let mut sink = Vec::new();
    reader.take(64 * 1024).read_to_end(&mut sink).ok();
    Ok(())
}

fn validate_unix_socket_path(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        path.as_os_str().as_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES,
        "Unix socket path is too long: {} (use a shorter SPAWNR_HOME)",
        path.display()
    );
    Ok(())
}

fn option_path(path: &Path) -> Result<&str> {
    let value = path
        .to_str()
        .with_context(|| format!("Cloud Hypervisor path is not UTF-8: {}", path.display()))?;
    ensure!(
        !value.contains([',', '"', '\n', '\r', '\0']),
        "Cloud Hypervisor path contains an unsupported character: {}",
        path.display()
    );
    Ok(value)
}

fn validate_machine_hostname(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 63
            && !name.starts_with('-')
            && !name.ends_with('-')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "invalid machine name {name:?} for guest hostname"
    );
    Ok(())
}

fn validate_mac_address(address: &str) -> Result<&str> {
    let octets = address
        .split(':')
        .map(|octet| {
            ensure!(octet.len() == 2, "invalid VM MAC address {address:?}");
            u8::from_str_radix(octet, 16).context("invalid hexadecimal octet in VM MAC address")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        octets.len() == 6 && octets[0] & 0x03 == 0x02,
        "VM MAC address must be a locally administered unicast address: {address:?}"
    );
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EnvironmentRecord;
    use crate::storage::{DiskSizes, Storage};
    use time::OffsetDateTime;
    use uuid::Uuid;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = Uuid::new_v4().simple().to_string();
            // AF_UNIX paths are capped at 108 bytes. Use the system's short
            // conventional test root even when a build wrapper sets TMPDIR to
            // a deeply nested path.
            let path = Path::new("/tmp").join(format!("sv-{}", &nonce[..8]));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

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
            name: "machine".into(),
            environment: EnvironmentRecord {
                reference: "example.invalid/env:1".into(),
                manifest_digest: None,
                base_cache_key: None,
            },
            repository: None,
            repository_dir: None,
            workspace_id: Uuid::new_v4(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            vsock_cid: 5432,
            mac_address: "02:00:00:00:00:01".into(),
        }
    }

    #[test]
    fn launch_command_has_three_domains_and_private_control_plane() {
        let Some(mkfs) = process::find_executable("mkfs.ext4") else {
            return;
        };
        let temporary = TestDir::new();
        let root = temporary.path().to_path_buf();
        let global = Paths::discover(Some(&root)).unwrap();
        global.ensure_layout().unwrap();
        let record = record();
        let paths = MachinePaths::for_record(&global, &record);
        paths.create(&record).unwrap();
        Storage::with_mkfs_ext4(mkfs)
            .create(
                &paths,
                &record,
                None,
                DiskSizes {
                    environment_bytes: 16 * 1024 * 1024,
                    workspace_bytes: 16 * 1024 * 1024,
                },
            )
            .unwrap();
        let kernel = temporary.path().join("kernel");
        fs::write(&kernel, b"kernel").unwrap();
        let vmm = Vmm::with_components("/bin/true", "/bin/true", kernel, None);
        let command = vmm.command(&paths, &record, 0).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        let joined = args.join(" ");
        assert!(joined.contains(paths.environment_disk.to_str().unwrap()));
        assert!(joined.contains(paths.workspace_disk.to_str().unwrap()));
        assert!(joined.contains(paths.vsock_socket.to_str().unwrap()));
        assert!(joined.contains("vhost_user=on"));
        assert!(joined.contains("--landlock"));
        assert!(joined.contains("init=/usr/libexec/spawnr-agent"));
        assert!(joined.contains("spawnr.machine_name=machine"));
        assert!(!joined.contains("SSH_AUTH_SOCK"));

        let network = vmm.network_command(&paths, 0).unwrap();
        let network_args = network
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(network_args.iter().any(|arg| arg == "--no-map-gw"));
        assert!(
            network_args
                .windows(2)
                .any(|args| args == ["--dns-forward", "169.254.0.53"])
        );
        assert!(
            network_args
                .windows(2)
                .any(|args| args == ["--tcp-ports", "none"])
        );
        assert!(
            network_args
                .windows(2)
                .any(|args| args == ["--udp-ports", "none"])
        );
    }

    #[test]
    fn rejects_overlong_unix_socket_paths() {
        let path = PathBuf::from(format!("/tmp/{}.sock", "x".repeat(110)));
        assert!(validate_unix_socket_path(&path).is_err());
    }

    #[test]
    fn rejects_machine_name_that_could_inject_a_kernel_parameter() {
        assert!(validate_machine_hostname("safe-name-1").is_ok());
        assert!(validate_machine_hostname("unsafe quiet").is_err());
        assert!(validate_machine_hostname("--unsafe").is_err());
    }

    #[test]
    fn accepts_only_locally_administered_unicast_mac_addresses() {
        assert!(validate_mac_address("02:00:00:00:00:01").is_ok());
        assert!(validate_mac_address("00:00:00:00:00:01").is_err());
        assert!(validate_mac_address("03:00:00:00:00:01").is_err());
        assert!(validate_mac_address("02:00:00:00:00:01,id=oops").is_err());
    }

    #[test]
    fn parses_host_stub_resolver_for_passt() {
        assert_eq!(
            parse_ipv4_resolver("search home\nnameserver 127.0.0.53 # stub\n").unwrap(),
            Ipv4Addr::new(127, 0, 0, 53)
        );
        assert!(parse_ipv4_resolver("nameserver ::1\n").is_err());
    }

    #[test]
    fn text_report_includes_remediation() {
        let report = DoctorReport {
            ready: false,
            checks: vec![DoctorCheck {
                name: "KVM".into(),
                ok: false,
                detail: "missing".into(),
                remedy: Some("enable it".into()),
            }],
        };
        let text = report.render_text();
        assert!(text.contains("✗ KVM"));
        assert!(text.contains("fix: enable it"));
    }
}
