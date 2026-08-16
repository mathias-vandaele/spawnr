use crate::account::User;
use anyhow::{Context, Result, bail, ensure};
use spawnr_protocol::SessionConfig;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub const GIT_CONFIG_FILE: &str = "gitconfig";
pub const GH_TOKEN_FILE: &str = "gh-token";
pub const IMAGE_ENV_FILE: &str = "image-env0";
pub const SSH_AGENT_SOCKET: &str = "ssh-agent.sock";
pub const SSH_KNOWN_HOSTS_FILE: &str = "ssh-known-hosts";
pub(crate) const MAX_ENVIRONMENT_ITEMS: usize = 512;
pub(crate) const MAX_ENVIRONMENT_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_ENV_FILE_BYTES: usize = MAX_ENVIRONMENT_BYTES + MAX_ENVIRONMENT_ITEMS;

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
    let image_environment = encode_image_environment(&config.image_env)?;

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
    let image_environment_path = session_dir.join(IMAGE_ENV_FILE);
    crate::write_private(&image_environment_path, &image_environment, 0o600)?;
    crate::account::chown(&image_environment_path, user)?;
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
    // Spawnr exposes one canonical session token. Never fall back to a token
    // baked into OCI metadata under either common variable name.
    command.env_remove("GITHUB_TOKEN");
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

pub fn image_environment(session_dir: &Path) -> Result<BTreeMap<String, String>> {
    let path = session_dir.join(IMAGE_ENV_FILE);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.file_type().is_file() && metadata.len() <= MAX_IMAGE_ENV_FILE_BYTES as u64,
        "invalid image environment file {}",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "image environment file is accessible by another account: {}",
        path.display()
    );
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take((MAX_IMAGE_ENV_FILE_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        contents.len() <= MAX_IMAGE_ENV_FILE_BYTES,
        "image environment file grew beyond its size limit"
    );
    decode_image_environment(&contents)
        .with_context(|| format!("decode image environment from {}", path.display()))
}

fn encode_image_environment(entries: &[String]) -> Result<Vec<u8>> {
    parse_image_environment(entries)?;
    let mut encoded = Vec::with_capacity(
        entries
            .iter()
            .map(|entry| entry.len().saturating_add(1))
            .sum(),
    );
    for entry in entries {
        encoded.extend_from_slice(entry.as_bytes());
        encoded.push(0);
    }
    Ok(encoded)
}

fn decode_image_environment(contents: &[u8]) -> Result<BTreeMap<String, String>> {
    if contents.is_empty() {
        return Ok(BTreeMap::new());
    }
    ensure!(
        contents.last() == Some(&0),
        "image environment is not NUL-terminated"
    );
    let entries = contents[..contents.len() - 1]
        .split(|byte| *byte == 0)
        .map(|entry| {
            std::str::from_utf8(entry)
                .context("image environment entry is not UTF-8")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    parse_image_environment(&entries)
}

fn parse_image_environment(entries: &[String]) -> Result<BTreeMap<String, String>> {
    ensure!(
        entries.len() <= MAX_ENVIRONMENT_ITEMS,
        "OCI image has too many environment variables"
    );
    let mut total = 0_usize;
    let mut environment = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        total = total
            .checked_add(entry.len().saturating_add(1))
            .context("OCI image environment size overflow")?;
        ensure!(
            total <= MAX_ENVIRONMENT_BYTES,
            "OCI image environment exceeds {MAX_ENVIRONMENT_BYTES} bytes"
        );
        let Some((name, value)) = entry.split_once('=') else {
            bail!("OCI image environment entry {index} has no '='");
        };
        ensure!(
            valid_image_environment_name(name),
            "OCI image environment entry {index} has an invalid variable name"
        );
        ensure!(
            !value.contains('\0'),
            "OCI image environment entry {index} contains NUL"
        );
        // OCI Config.Env is ordered. Replacing an existing key implements the
        // runtime's last-entry-wins behavior without exposing duplicate names
        // to execve(2).
        environment.insert(name.to_owned(), value.to_owned());
    }
    Ok(environment)
}

fn valid_image_environment_name(name: &str) -> bool {
    // OCI Config.Env becomes an execve(2) environment. It is not constrained
    // to names that a shell accepts on the left side of an assignment.
    !name.is_empty() && !name.contains(['=', '\0'])
}

pub(crate) fn valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
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

    #[test]
    fn image_environment_is_bounded_and_last_entry_wins() {
        let entries = vec![
            "PATH=/first".into(),
            "EMPTY=".into(),
            "WITH_EQUALS=a=b".into(),
            "PATH=/last".into(),
        ];
        let encoded = encode_image_environment(&entries).unwrap();
        let decoded = decode_image_environment(&encoded).unwrap();
        assert_eq!(decoded["PATH"], "/last");
        assert_eq!(decoded["EMPTY"], "");
        assert_eq!(decoded["WITH_EQUALS"], "a=b");

        assert!(parse_image_environment(&["MISSING_EQUALS".into()]).is_err());
        assert!(parse_image_environment(&["2VALID=value".into()]).is_ok());
        assert!(parse_image_environment(&["VALID-DASH=value".into()]).is_ok());
        assert!(parse_image_environment(&["org.example.option=value".into()]).is_ok());
        assert!(parse_image_environment(&["=empty-name".into()]).is_err());
        assert!(parse_image_environment(&["BAD\0NAME=value".into()]).is_err());
        assert!(parse_image_environment(&["BAD=value\0tail".into()]).is_err());
        assert!(decode_image_environment(b"PATH=/bin").is_err());

        let too_many = vec!["A=value".to_owned(); MAX_ENVIRONMENT_ITEMS + 1];
        assert!(parse_image_environment(&too_many).is_err());
        let too_large = format!("A={}", "x".repeat(MAX_ENVIRONMENT_BYTES));
        assert!(parse_image_environment(&[too_large]).is_err());
    }
}
