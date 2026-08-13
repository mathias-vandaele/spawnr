use crate::account::User;
use anyhow::{Context, Result, bail};
use spawnr_protocol::SSH_AGENT_PORT;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use vsock::{VMADDR_CID_HOST, VsockStream};

static PROXY: OnceLock<Proxy> = OnceLock::new();
static INITIALIZE: Mutex<()> = Mutex::new(());

struct Proxy {
    enabled: AtomicBool,
    path: PathBuf,
}

pub fn set_enabled(path: PathBuf, user: &User, enabled: bool) -> Result<()> {
    let _initialize = INITIALIZE
        .lock()
        .map_err(|_| anyhow::anyhow!("SSH-agent proxy lock is poisoned"))?;
    let proxy = if let Some(proxy) = PROXY.get() {
        if proxy.path != path {
            bail!("SSH-agent proxy was initialized at a different session path");
        }
        proxy
    } else {
        start(path, user)?
    };
    proxy.enabled.store(enabled, Ordering::Release);
    Ok(())
}

pub fn is_enabled() -> bool {
    PROXY
        .get()
        .is_some_and(|proxy| proxy.enabled.load(Ordering::Acquire))
}

fn start(path: PathBuf, user: &User) -> Result<&'static Proxy> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(&path)
                .with_context(|| format!("remove stale socket {}", path.display()))?;
        }
        Ok(_) => bail!(
            "refusing to replace non-socket session path {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind guest SSH-agent socket {}", path.display()))?;
    crate::session::secure_socket_permissions(&path, user)?;
    let proxy = PROXY.get_or_init(|| Proxy {
        enabled: AtomicBool::new(false),
        path: path.clone(),
    });
    thread::Builder::new()
        .name("spawnr-ssh-agent".into())
        .spawn(move || accept_loop(listener))
        .context("spawn SSH-agent proxy")?;
    Ok(proxy)
}

fn accept_loop(listener: UnixListener) {
    for connection in listener.incoming() {
        match connection {
            Ok(stream) if is_enabled() => {
                let _ = thread::Builder::new()
                    .name("spawnr-ssh-agent-client".into())
                    .spawn(move || {
                        if let Err(error) = proxy(stream) {
                            eprintln!("spawnr-agent: SSH-agent forwarding failed: {error:#}");
                        }
                    });
            }
            Ok(_) => {}
            Err(error) => eprintln!("spawnr-agent: SSH-agent accept failed: {error}"),
        }
    }
}

fn proxy(mut guest: UnixStream) -> Result<()> {
    let mut host = VsockStream::connect_with_cid_port(VMADDR_CID_HOST, SSH_AGENT_PORT)
        .context("connect to host SSH-agent capability")?;
    let mut guest_read = guest.try_clone()?;
    let mut host_write = host.try_clone()?;
    let upload = thread::spawn(move || io::copy(&mut guest_read, &mut host_write));
    io::copy(&mut host, &mut guest).context("copy SSH-agent response")?;
    let _ = upload.join();
    Ok(())
}
