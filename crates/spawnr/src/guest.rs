use anyhow::{Context, Result, bail};
use spawnr_protocol::{
    Request, Response, StreamKind, read_json, read_stream_frame, write_json, write_stream_frame,
};
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct GuestClient {
    vsock_socket: PathBuf,
}

impl GuestClient {
    pub fn new(vsock_socket: PathBuf) -> Self {
        Self { vsock_socket }
    }

    pub fn request_timeout(&self, request: &Request, timeout: Duration) -> Result<Response> {
        let mut stream = connect_hybrid_vsock(&self.vsock_socket)?;
        stream
            .set_read_timeout(Some(timeout))
            .context("set guest response deadline")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("set guest request deadline")?;
        write_json(&mut stream, request)?;
        read_json(&mut stream)
    }

    pub fn wait_healthy(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let attempt = remaining.min(Duration::from_secs(1));
            match self.request_timeout(&Request::Health, attempt) {
                Ok(Response::Health { .. }) => return Ok(()),
                Ok(other) => last_error = Some(anyhow::anyhow!("unexpected response: {other:?}")),
                Err(error) => last_error = Some(error),
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("guest did not answer")))
            .context("guest agent did not become healthy before the 30 second timeout")
    }

    pub fn interactive_exec(&self, request: Request) -> Result<i32> {
        let mut stream = connect_hybrid_vsock(&self.vsock_socket)?;
        write_json(&mut stream, &request)?;
        match read_json::<Response>(&mut stream)? {
            Response::ExecReady => {}
            Response::Error { message } => bail!("guest refused interactive session: {message}"),
            response => bail!("guest returned an unexpected response: {response:?}"),
        }

        let _terminal = TerminalMode::raw_if_terminal()?;
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        let signals = install_terminal_signal_handlers()?;
        let signal_writer = Arc::clone(&writer);
        thread::spawn(move || forward_terminal_signals(signal_writer, signals));
        let input_writer = Arc::clone(&writer);
        thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                let count = match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let Ok(mut socket) = input_writer.lock() else {
                    break;
                };
                if write_stream_frame(&mut *socket, StreamKind::Stdin, &buffer[..count]).is_err() {
                    break;
                }
            }
        });

        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        loop {
            let (kind, payload) = read_stream_frame(&mut stream)?;
            match kind {
                StreamKind::Stdout => {
                    stdout.write_all(&payload)?;
                    stdout.flush()?;
                }
                StreamKind::Stderr => {
                    stderr.write_all(&payload)?;
                    stderr.flush()?;
                }
                StreamKind::Exit => {
                    if payload.len() != 4 {
                        bail!("guest sent malformed exit status");
                    }
                    return Ok(i32::from_be_bytes(payload.try_into().unwrap()));
                }
                _ => bail!("guest sent invalid host-bound stream frame {kind:?}"),
            }
        }
    }
}

const TERMINAL_SIGNALS: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGWINCH];

fn install_terminal_signal_handlers() -> Result<libc::sigset_t> {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the provided storage.
    if unsafe { libc::sigemptyset(set.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("initialize terminal signal set");
    }
    // SAFETY: initialized above.
    let mut set = unsafe { set.assume_init() };
    for signal in TERMINAL_SIGNALS {
        // SAFETY: set is initialized and signal is a valid constant.
        if unsafe { libc::sigaddset(&mut set, signal) } != 0 {
            return Err(std::io::Error::last_os_error()).context("add terminal signal");
        }
    }
    // Blocking before the forwarding thread is created prevents delivery to
    // either thread through the host CLI's default handlers.
    let status = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status)).context("block terminal signals");
    }
    Ok(set)
}

fn forward_terminal_signals(writer: Arc<Mutex<UnixStream>>, set: libc::sigset_t) {
    loop {
        let mut signal = 0;
        // SAFETY: set is initialized, signal points to writable storage, and
        // sigwait blocks this dedicated thread until one selected signal arrives.
        if unsafe { libc::sigwait(&set, &mut signal) } != 0 {
            break;
        }
        let frame = if signal == libc::SIGWINCH {
            let (rows, cols) = terminal_size();
            let mut payload = [0_u8; 4];
            payload[..2].copy_from_slice(&rows.to_be_bytes());
            payload[2..].copy_from_slice(&cols.to_be_bytes());
            (StreamKind::Resize, payload.to_vec())
        } else {
            (StreamKind::Signal, signal.to_be_bytes().to_vec())
        };
        let Ok(mut socket) = writer.lock() else {
            break;
        };
        if write_stream_frame(&mut *socket, frame.0, &frame.1).is_err() {
            break;
        }
    }
}

fn connect_hybrid_vsock(path: &Path) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("connect to VM vsock at {}", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("set vsock handshake timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("set vsock handshake write timeout")?;
    writeln!(stream, "CONNECT {}", spawnr_protocol::CONTROL_PORT)?;
    stream.flush()?;

    let mut reply = Vec::with_capacity(32);
    let mut byte = [0_u8; 1];
    while reply.len() < 128 {
        stream
            .read_exact(&mut byte)
            .context("read vsock handshake")?;
        reply.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let reply = std::str::from_utf8(&reply).context("vsock returned non-UTF-8 handshake")?;
    if !reply.starts_with("OK ") || !reply.ends_with('\n') {
        bail!("vsock rejected port connection: {}", reply.trim_end());
    }
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(stream)
}

struct TerminalMode {
    terminal: Option<(i32, libc::termios)>,
}

impl TerminalMode {
    fn raw_if_terminal() -> Result<Self> {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            return Ok(Self { terminal: None });
        }
        let fd = stdin.as_raw_fd();
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the termios value for a valid stdin fd.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("read terminal mode");
        }
        // SAFETY: initialized by the successful tcgetattr call above.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // SAFETY: cfmakeraw only mutates the provided initialized struct.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: valid fd and initialized struct.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error()).context("enable raw terminal mode");
        }
        Ok(Self {
            terminal: Some((fd, original)),
        })
    }
}

impl Drop for TerminalMode {
    fn drop(&mut self) {
        if let Some((fd, original)) = &self.terminal {
            // SAFETY: the fd and termios came from a successful tcgetattr.
            unsafe { libc::tcsetattr(*fd, libc::TCSANOW, original) };
        }
    }
}

pub fn terminal_size() -> (u16, u16) {
    let output = File::open("/dev/tty").ok().and_then(|file| {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: size points to writable storage and file is a valid fd.
        (unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } == 0)
            .then_some((size.ws_row, size.ws_col))
    });
    output.unwrap_or((24, 80))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use uuid::Uuid;

    #[test]
    fn performs_cloud_hypervisor_hybrid_handshake() {
        let temporary = std::env::temp_dir().join(format!("spawnr-vsock-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&temporary).unwrap();
        let socket = temporary.join("vsock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut line = [0_u8; 14];
            peer.read_exact(&mut line).unwrap();
            assert_eq!(&line, b"CONNECT 19870\n");
            peer.write_all(b"OK 40001\n").unwrap();
        });
        connect_hybrid_vsock(&socket).unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(temporary).unwrap();
    }
}
