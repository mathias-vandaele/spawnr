use anyhow::{Context, Result, bail, ensure};
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

pub fn lookup(name: &str) -> Result<User> {
    ensure_safe_account_name(name)?;
    let c_name = CString::new(name).context("account name contains NUL")?;
    // SAFETY: getpwnam returns either null or a pointer to libc-managed static
    // storage. We copy every required field before making another NSS call.
    let entry = unsafe { libc::getpwnam(c_name.as_ptr()) };
    if entry.is_null() {
        bail!("user {name:?} does not exist");
    }
    // SAFETY: a non-null getpwnam result points to a valid passwd structure.
    let entry = unsafe { &*entry };
    let home = c_path(entry.pw_dir, "home directory")?;
    let shell = c_path(entry.pw_shell, "login shell")?;
    Ok(User {
        name: name.to_owned(),
        uid: entry.pw_uid,
        gid: entry.pw_gid,
        home,
        shell,
    })
}

pub fn ensure_development_user(name: &str) -> Result<User> {
    ensure_safe_account_name(name)?;
    if let Ok(user) = lookup(name) {
        install_sudo_policy(&user)?;
        return Ok(user);
    }

    let useradd = find_absolute(&["/usr/sbin/useradd", "/sbin/useradd"])
        .context("useradd is required to create the development account")?;
    let status = Command::new(useradd)
        .args([
            "--create-home",
            "--user-group",
            "--shell",
            "/bin/bash",
            "--",
        ])
        .arg(name)
        .status()
        .context("run useradd")?;
    ensure!(status.success(), "useradd failed with {status}");
    let user = lookup(name)?;
    install_sudo_policy(&user)?;
    Ok(user)
}

pub fn chown(path: &Path, user: &User) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).context("path contains NUL")?;
    // SAFETY: path is a valid C string and chown has no memory safety preconditions.
    if unsafe { libc::chown(path.as_ptr(), user.uid, user.gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("chown guest path");
    }
    Ok(())
}

fn install_sudo_policy(user: &User) -> Result<()> {
    let sudoers = Path::new("/etc/sudoers.d");
    if !sudoers.is_dir() {
        fs::create_dir_all(sudoers).context("create sudo policy directory")?;
    }
    let path = sudoers.join("90-spawnr-dev");
    let contents = format!("{} ALL=(ALL:ALL) NOPASSWD: ALL\n", user.name);
    crate::write_private(&path, contents.as_bytes(), 0o440)?;
    if Path::new("/usr/bin/sudo").is_file() {
        return Ok(());
    }
    install_sudo_shim()?;
    Ok(())
}

fn install_sudo_shim() -> Result<()> {
    let path = Path::new("/usr/local/bin/sudo");
    fs::create_dir_all(path.parent().context("sudo fallback has no parent")?)
        .context("create sudo fallback directory")?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(path)?,
        Ok(_) => bail!(
            "refusing to replace existing sudo fallback at {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect sudo fallback"),
    }
    std::os::unix::fs::symlink("/usr/libexec/spawnr-agent", path)
        .context("install Spawnr sudo fallback")
}

fn c_path(pointer: *const libc::c_char, description: &str) -> Result<PathBuf> {
    if pointer.is_null() {
        bail!("account has no {description}");
    }
    // SAFETY: NSS passwd string pointers are NUL-terminated for a valid entry.
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(unsafe {
        CStr::from_ptr(pointer).to_bytes()
    })))
}

fn ensure_safe_account_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "invalid development account name {name:?}"
    );
    Ok(())
}

fn find_absolute<'a>(candidates: &'a [&'a str]) -> Option<&'a Path> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
        .map(Path::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_are_tightly_validated() {
        assert!(ensure_safe_account_name("dev").is_ok());
        assert!(ensure_safe_account_name("dev-user_2").is_ok());
        assert!(ensure_safe_account_name("../../root").is_err());
        assert!(ensure_safe_account_name("dev user").is_err());
    }
}
