use crate::account::User;
use anyhow::{Context, Result, ensure};
use spawnr_protocol::SessionConfig;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub const GIT_CONFIG_FILE: &str = "gitconfig";
pub const GH_TOKEN_FILE: &str = "gh-token";
pub const SSH_AGENT_SOCKET: &str = "ssh-agent.sock";
pub const SSH_KNOWN_HOSTS_FILE: &str = "ssh-known-hosts";

pub fn configure(session_dir: &Path, user: &User, config: SessionConfig) -> Result<String> {
    crate::mount::assert_session_is_tmpfs(session_dir)?;
    validate_optional("Git user name", config.git_user_name.as_deref(), 1024)?;
    validate_optional("Git user email", config.git_user_email.as_deref(), 1024)?;
    validate_optional(
        "Git signing key",
        config.git_signing_key.as_deref(),
        16 * 1024,
    )?;
    validate_optional("GitHub token", config.gh_token.as_deref(), 64 * 1024)?;
    validate_known_hosts(config.ssh_known_hosts.as_deref())?;

    write_git_config(session_dir, user, &config)?;
    replace_secret(
        &session_dir.join(GH_TOKEN_FILE),
        user,
        config.gh_token.as_deref(),
    )?;
    replace_secret(
        &session_dir.join(SSH_KNOWN_HOSTS_FILE),
        user,
        config.ssh_known_hosts.as_deref(),
    )?;
    crate::agent_proxy::set_enabled(session_dir.join(SSH_AGENT_SOCKET), user, config.ssh_agent)?;

    Ok("session capabilities configured in ephemeral tmpfs".into())
}

fn write_git_config(session_dir: &Path, user: &User, config: &SessionConfig) -> Result<()> {
    let mut contents = String::from("[user]\n\tuseConfigOnly = true\n");
    if let Some(name) = &config.git_user_name {
        contents.push_str("\tname = ");
        contents.push_str(&quote_git_config(name));
        contents.push('\n');
    }
    if let Some(email) = &config.git_user_email {
        contents.push_str("\temail = ");
        contents.push_str(&quote_git_config(email));
        contents.push('\n');
    }
    if let Some(key) = &config.git_signing_key {
        contents.push_str("\tsigningKey = ");
        contents.push_str(&quote_git_config(key));
        contents.push_str("\n[gpg]\n\tformat = ssh\n");
    }
    let path = session_dir.join(GIT_CONFIG_FILE);
    crate::write_private(&path, contents.as_bytes(), 0o600)?;
    crate::account::chown(&path, user)?;
    Ok(())
}

fn replace_secret(path: &Path, user: &User, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            crate::write_private(path, value.as_bytes(), 0o600)?;
            crate::account::chown(path, user)?;
        }
        None => match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        },
    }
    Ok(())
}

pub fn apply_environment(command: &mut std::process::Command, session_dir: &Path) -> Result<()> {
    command.env("GIT_CONFIG_GLOBAL", session_dir.join(GIT_CONFIG_FILE));
    let agent = session_dir.join(SSH_AGENT_SOCKET);
    if agent.exists() && crate::agent_proxy::is_enabled() {
        command.env("SSH_AUTH_SOCK", agent);
    } else {
        command.env_remove("SSH_AUTH_SOCK");
    }
    let known_hosts = session_dir.join(SSH_KNOWN_HOSTS_FILE);
    if known_hosts.is_file() {
        command.env(
            "GIT_SSH_COMMAND",
            format!(
                "ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o GlobalKnownHostsFile=/dev/null -o UserKnownHostsFile={}",
                known_hosts.display()
            ),
        );
    } else {
        command.env_remove("GIT_SSH_COMMAND");
    }
    let token = session_dir.join(GH_TOKEN_FILE);
    match fs::read_to_string(&token) {
        Ok(value) => {
            command.env("GH_TOKEN", value);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            command.env_remove("GH_TOKEN");
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", token.display())),
    }
    Ok(())
}

fn validate_known_hosts(value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        ensure!(!value.is_empty(), "SSH known-hosts must not be empty");
        ensure!(value.len() <= 1024 * 1024, "SSH known-hosts exceeds 1 MiB");
        ensure!(
            !value.contains(['\0', '\r']) && value.lines().all(|line| line.len() <= 64 * 1024),
            "SSH known-hosts contains an invalid record"
        );
    }
    Ok(())
}

fn validate_optional(label: &str, value: Option<&str>, maximum: usize) -> Result<()> {
    if let Some(value) = value {
        ensure!(!value.is_empty(), "{label} must not be empty");
        ensure!(value.len() <= maximum, "{label} exceeds {maximum} bytes");
        ensure!(
            !value
                .chars()
                .any(|character| character == '\0' || character.is_control()),
            "{label} contains a control character"
        );
    }
    Ok(())
}

fn quote_git_config(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

pub fn secure_socket_permissions(path: &Path, user: &User) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    crate::account::chown(path, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_values_are_quoted_without_new_syntax() {
        assert_eq!(quote_git_config("A \\\" Name"), "\"A \\\\\\\" Name\"");
    }

    #[test]
    fn credential_values_reject_control_characters() {
        assert!(validate_optional("token", Some("abc def"), 20).is_ok());
        assert!(validate_optional("token", Some("abc\ndef"), 20).is_err());
        assert!(validate_optional("token", Some("abc\0def"), 20).is_err());
    }

    #[test]
    fn known_hosts_allows_records_but_not_nul() {
        assert!(validate_known_hosts(Some("github.com ssh-ed25519 AAAA\n")).is_ok());
        assert!(validate_known_hosts(Some("github.com\0bad\n")).is_err());
    }
}
