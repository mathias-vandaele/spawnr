use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Spawnr's private guest control plane")]
struct Arguments {
    /// Name reported to the host. SPAWNR_MACHINE_NAME is the image-friendly alternative.
    #[arg(long, env = "SPAWNR_MACHINE_NAME", default_value = "spawnr")]
    machine_name: String,

    /// Prepare /workspace and /run/spawnr before serving requests.
    #[arg(long)]
    initialize_workspace: bool,

    /// AF_VSOCK control port.
    #[arg(long, default_value_t = spawnr_protocol::CONTROL_PORT)]
    port: u32,

    /// Development account used for repository and command operations.
    #[arg(long, default_value = "dev")]
    user: String,

    /// Workspace mount point.
    #[arg(long, default_value = "/workspace")]
    workspace: PathBuf,

    /// Session tmpfs mount point.
    #[arg(long, default_value = "/run/spawnr")]
    session_dir: PathBuf,
}

fn main() {
    if invoked_as_sudo() {
        sudo_main();
    }
    if let Err(error) = run() {
        eprintln!("spawnr-agent: {error:#}");
        std::process::exit(1);
    }
}

fn invoked_as_sudo() -> bool {
    std::env::args_os()
        .next()
        .as_deref()
        .and_then(|argument| std::path::Path::new(argument).file_name())
        .is_some_and(|name| name == "sudo")
}

fn sudo_main() -> ! {
    // The injected guest binary is mode 4755 specifically to provide the
    // documented passwordless-root capability inside this single-user VM.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("sudo: Spawnr agent is not installed setuid-root");
        std::process::exit(1);
    }
    // Convert the setuid execution credential into an ordinary root process
    // before launching the requested program. Several administrative tools
    // (including apt helpers) deliberately consult the real UID as well as
    // the effective UID.
    // SAFETY: PID-local credential changes, executed while euid is root.
    let credentials_reset = unsafe {
        libc::setgroups(0, std::ptr::null()) == 0
            && libc::setresgid(0, 0, 0) == 0
            && libc::setresuid(0, 0, 0) == 0
    };
    if !credentials_reset {
        eprintln!(
            "sudo: cannot assume root credentials: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }
    let mut arguments = std::env::args_os().skip(1).peekable();
    while matches!(
        arguments.peek().and_then(|v| v.to_str()),
        Some("-E" | "--preserve-env")
    ) {
        arguments.next();
    }
    let Some(program) = arguments.next() else {
        eprintln!("usage: sudo COMMAND [ARGUMENT ...]");
        std::process::exit(2);
    };
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(program).args(arguments).exec();
    eprintln!("sudo: {error}");
    std::process::exit(126);
}

fn run() -> Result<()> {
    let mut arguments = Arguments::parse();
    let pid_one = std::process::id() == 1;
    if pid_one {
        arguments.initialize_workspace = true;
        if arguments.machine_name == "spawnr" {
            arguments.machine_name =
                machine_name_from_cmdline().unwrap_or_else(|| "spawnr".to_owned());
        }
        set_hostname(&arguments.machine_name).context("set guest hostname")?;
        spawnr_agent::initialize_pid_one()
            .context("initialize guest process and network mounts")?;
    }
    let config = spawnr_agent::Config {
        machine_name: arguments.machine_name,
        control_port: arguments.port,
        dev_user: arguments.user,
        workspace: arguments.workspace,
        session_dir: arguments.session_dir,
    };
    if arguments.initialize_workspace {
        spawnr_agent::initialize(&config).context("initialize guest mounts and account")?;
    }
    if pid_one {
        spawnr_agent::configure_pid_one_network().context("configure guest networking")?;
        spawnr_agent::start_pid_one_reaper().context("start guest orphan reaper")?;
    }
    spawnr_agent::serve(config)
}

fn set_hostname(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 63
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "invalid Spawnr machine name {name:?}"
    );
    // SAFETY: `name` is a live byte slice and sethostname copies exactly the
    // supplied number of bytes into the kernel's UTS state.
    let status = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("sethostname");
    }
    Ok(())
}

fn machine_name_from_cmdline() -> Option<String> {
    let command_line = std::fs::read_to_string("/proc/cmdline").ok()?;
    command_line
        .split_ascii_whitespace()
        .find_map(|value| value.strip_prefix("spawnr.machine_name="))
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 63
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        .map(str::to_owned)
}
