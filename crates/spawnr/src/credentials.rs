use anyhow::{Context, Result};
use spawnr_protocol::SessionConfig;
use std::env;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SSH_AGENT_VSOCK_PORT: u32 = 19_871;

#[derive(Debug, Clone)]
pub struct HostCredentials {
    pub session: SessionConfig,
    ssh_auth_sock: Option<PathBuf>,
}

impl HostCredentials {
    pub fn collect() -> Self {
        let ssh_auth_sock = valid_agent_socket();
        Self {
            session: SessionConfig {
                git_user_name: git_value("user.name"),
                git_user_email: git_value("user.email"),
                git_signing_key: ssh_signing_key(),
                gh_token: github_token(),
                ssh_known_hosts: ssh_known_hosts(),
                ssh_agent: ssh_auth_sock.is_some(),
            },
            ssh_auth_sock,
        }
    }

    /// Expose exactly one host capability to Cloud Hypervisor's hybrid-vsock
    /// backend. Connecting to `vsock_base_<port>` follows this symlink to the
    /// user's agent. No key material is copied.
    pub fn expose_ssh_agent(&self, vsock_base: &Path) -> Result<Option<AgentLink>> {
        let Some(agent) = &self.ssh_auth_sock else {
            return Ok(None);
        };
        let link = PathBuf::from(format!("{}_{}", vsock_base.display(), SSH_AGENT_VSOCK_PORT));
        match fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&link).with_context(|| {
                    format!("remove stale SSH-agent capability {}", link.display())
                })?;
            }
            Ok(_) => anyhow::bail!(
                "refusing to replace non-symlink session endpoint {}",
                link.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect SSH-agent capability endpoint"),
        }
        std::os::unix::fs::symlink(agent, &link).with_context(|| {
            format!(
                "expose SSH agent {} through {}",
                agent.display(),
                link.display()
            )
        })?;
        Ok(Some(AgentLink {
            path: link,
            persistent: false,
        }))
    }

    pub fn ssh_auth_sock(&self) -> Option<&Path> {
        self.ssh_auth_sock.as_deref()
    }
}

pub struct AgentLink {
    path: PathBuf,
    persistent: bool,
}

impl AgentLink {
    /// Keep the capability endpoint until VM shutdown. The endpoint is only a
    /// symlink to a host-owned socket; it contains no credential material.
    pub fn persist(mut self) {
        self.persistent = true;
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        if !self.persistent {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn valid_agent_socket() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os("SSH_AUTH_SOCK")?);
    let metadata = fs::metadata(&path).ok()?;
    metadata.file_type().is_socket().then_some(path)
}

fn git_value(key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--global", "--get", key])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    one_safe_line(&output.stdout)
}

fn ssh_signing_key() -> Option<String> {
    if git_value("gpg.format").as_deref() != Some("ssh") {
        return None;
    }
    git_value("user.signingkey")
}

fn github_token() -> Option<String> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Some(token) = env::var_os(name).and_then(|value| value.into_string().ok())
            && !token.is_empty()
            && !token.contains(['\n', '\r', '\0'])
        {
            return Some(token);
        }
    }
    None
}

fn ssh_known_hosts() -> Option<String> {
    const MAX_KNOWN_HOSTS_BYTES: usize = 1024 * 1024;
    let mut paths = vec![PathBuf::from("/etc/ssh/ssh_known_hosts")];
    if let Some(home) = env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".ssh/known_hosts"));
    }
    let mut output = String::new();
    for path in paths {
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file()
            || metadata.len() as usize > MAX_KNOWN_HOSTS_BYTES.saturating_sub(output.len())
        {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if contents.contains(['\0', '\r'])
            || contents.lines().any(|line| line.len() > 64 * 1024)
            || output.len() + contents.len() > MAX_KNOWN_HOSTS_BYTES
        {
            continue;
        }
        output.push_str(&contents);
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    (!output.is_empty()).then_some(output)
}

fn one_safe_line(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes).ok()?.trim().to_owned();
    (!value.is_empty() && !value.contains(['\n', '\r', '\0'])).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_multiline_config() {
        assert_eq!(one_safe_line(b"safe\nunsafe\n"), None);
        assert_eq!(one_safe_line(b" Person \n"), Some("Person".into()));
    }
}
