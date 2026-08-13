//! Versioned, length-prefixed protocol shared by Spawnr and its guest agent.
//!
//! The transport is deliberately private AF_VSOCK. JSON keeps the V1 protocol
//! inspectable while the four-byte length prefix permits binary-safe messages.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub const CONTROL_PORT: u32 = 19_870;
/// Guest connections to this host port are forwarded to the user's SSH agent.
pub const SSH_AGENT_PORT: u32 = 19_871;
pub const MAX_CONTROL_FRAME: usize = 8 * 1024 * 1024;
pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Health,
    ConfigureSession(SessionConfig),
    CloneRepository {
        repository: String,
        destination: String,
    },
    Exec {
        argv: Vec<String>,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
        tty: bool,
        rows: u16,
        cols: u16,
    },
    WorkspaceStatus {
        repository_path: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConfig {
    pub git_user_name: Option<String>,
    pub git_user_email: Option<String>,
    pub git_signing_key: Option<String>,
    pub gh_token: Option<String>,
    /// Host-owned public SSH host keys, copied only into guest session tmpfs.
    pub ssh_known_hosts: Option<String>,
    pub ssh_agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Health {
        protocol: u16,
        machine_name: String,
        workspace_mounted: bool,
    },
    Ok {
        message: String,
    },
    CommandResult {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    WorkspaceStatus {
        clean: bool,
        porcelain: String,
    },
    ExecReady,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    Stdin = 0,
    Stdout = 1,
    Stderr = 2,
    Resize = 3,
    Signal = 4,
    Exit = 5,
}

impl TryFrom<u8> for StreamKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Stdin,
            1 => Self::Stdout,
            2 => Self::Stderr,
            3 => Self::Resize,
            4 => Self::Signal,
            5 => Self::Exit,
            _ => bail!("unknown stream frame kind {value}"),
        })
    }
}

pub fn write_json<T: Serialize>(mut writer: impl Write, value: &T) -> Result<()> {
    let data = serde_json::to_vec(value).context("serialize control message")?;
    if data.len() > MAX_CONTROL_FRAME {
        bail!("control message is too large: {} bytes", data.len());
    }
    writer
        .write_all(&(data.len() as u32).to_be_bytes())
        .context("write control frame length")?;
    writer
        .write_all(&data)
        .context("write control frame body")?;
    writer.flush().context("flush control frame")?;
    Ok(())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(mut reader: impl Read) -> Result<T> {
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .context("read control frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL_FRAME {
        bail!("peer sent an oversized control frame: {length} bytes");
    }
    let mut data = vec![0_u8; length];
    reader
        .read_exact(&mut data)
        .context("read control frame body")?;
    serde_json::from_slice(&data).context("decode control message")
}

pub fn write_stream_frame(mut writer: impl Write, kind: StreamKind, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_CONTROL_FRAME {
        bail!("stream frame is too large: {} bytes", payload.len());
    }
    writer.write_all(&[kind as u8])?;
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_stream_frame(mut reader: impl Read) -> Result<(StreamKind, Vec<u8>)> {
    let mut header = [0_u8; 5];
    reader.read_exact(&mut header)?;
    let kind = StreamKind::try_from(header[0])?;
    let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    if length > MAX_CONTROL_FRAME {
        bail!("peer sent an oversized stream frame: {length} bytes");
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok((kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_is_framed() {
        let request = Request::Health;
        let mut bytes = Vec::new();
        write_json(&mut bytes, &request).unwrap();
        assert_eq!(read_json::<Request>(&bytes[..]).unwrap(), request);
    }

    #[test]
    fn rejects_oversized_frame_without_allocating_it() {
        let bytes = ((MAX_CONTROL_FRAME + 1) as u32).to_be_bytes();
        let error = read_json::<Request>(&bytes[..]).unwrap_err().to_string();
        assert!(error.contains("oversized"));
    }
}
