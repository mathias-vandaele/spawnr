use anyhow::{Context, Result, bail, ensure};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const WORKSPACE_LABEL: &str = "SPAWNR_WORKSPACE";

pub fn ensure_workspace_mounted(workspace: &Path) -> Result<()> {
    fs::create_dir_all(workspace)
        .with_context(|| format!("create workspace mount point {}", workspace.display()))?;
    if !exact_mount(workspace)? {
        let device = resolve_label(WORKSPACE_LABEL)?;
        mount_ext4(&device, workspace)?;
    }
    assert_workspace_is_separate(workspace)?;
    let expected_device = fs::canonicalize(resolve_label(WORKSPACE_LABEL)?)?;
    let entry = mount_entry(workspace)?.context("workspace mount disappeared")?;
    ensure!(
        entry.fs_type == "ext4",
        "workspace filesystem is {}, not ext4",
        entry.fs_type
    );
    let mounted_device = fs::canonicalize(&entry.source).with_context(|| {
        format!(
            "resolve mounted workspace device {}",
            entry.source.display()
        )
    })?;
    ensure!(
        mounted_device == expected_device,
        "{} is mounted from {}, not the device labeled {}",
        workspace.display(),
        mounted_device.display(),
        WORKSPACE_LABEL
    );
    Ok(())
}

pub fn assert_workspace_is_separate(workspace: &Path) -> Result<()> {
    let workspace_meta = fs::metadata(workspace)
        .with_context(|| format!("stat workspace {}", workspace.display()))?;
    ensure!(
        workspace_meta.is_dir(),
        "{} is not a directory",
        workspace.display()
    );
    ensure!(
        exact_mount(workspace)?,
        "{} is not an exact mount point",
        workspace.display()
    );
    let root_meta = fs::metadata("/").context("stat root filesystem")?;
    ensure!(
        workspace_meta.dev() != root_meta.dev(),
        "{} shares root filesystem device {}; refusing to blur environment and workspace",
        workspace.display(),
        root_meta.dev()
    );
    Ok(())
}

pub fn workspace_is_separate(workspace: &Path) -> bool {
    assert_workspace_is_separate(workspace).is_ok()
}

pub fn ensure_session_tmpfs(session: &Path) -> Result<()> {
    fs::create_dir_all(session)
        .with_context(|| format!("create session mount point {}", session.display()))?;
    if !exact_mount(session)? {
        mount_tmpfs(session)?;
    }
    assert_session_is_tmpfs(session)
}

pub fn assert_session_is_tmpfs(session: &Path) -> Result<()> {
    ensure!(
        exact_mount(session)?,
        "{} is not an exact mount point",
        session.display()
    );
    let entry = mount_entry(session)?.context("session mount disappeared")?;
    ensure!(
        entry.fs_type == "tmpfs",
        "{} is {}, not tmpfs",
        session.display(),
        entry.fs_type
    );
    let options = format!(",{},", entry.options);
    ensure!(options.contains(",rw,"), "session tmpfs is not writable");
    ensure!(options.contains(",nosuid,"), "session tmpfs lacks nosuid");
    ensure!(options.contains(",nodev,"), "session tmpfs lacks nodev");
    Ok(())
}

fn resolve_label(label: &str) -> Result<PathBuf> {
    let by_label = PathBuf::from("/dev/disk/by-label").join(label);
    if by_label.exists() {
        return fs::canonicalize(&by_label)
            .with_context(|| format!("resolve workspace label at {}", by_label.display()));
    }
    for blkid in ["/usr/sbin/blkid", "/sbin/blkid"] {
        if !Path::new(blkid).is_file() {
            continue;
        }
        let output = Command::new(blkid)
            .args(["-L", label])
            .output()
            .context("run util-linux blkid")?;
        if output.status.success() {
            let path = std::str::from_utf8(&output.stdout)?.trim();
            if safe_block_device(path) {
                return Ok(PathBuf::from(path));
            }
        }
    }

    // BusyBox deliberately implements only the portable listing form, not
    // util-linux's `-L`. Parse its bounded output and require an exact label.
    let busybox = Path::new("/usr/libexec/spawnr-busybox");
    ensure!(
        busybox.is_file(),
        "cannot resolve {label}: util-linux blkid and Spawnr BusyBox are unavailable"
    );
    let output = Command::new(busybox)
        .args(["blkid", "/dev/vda", "/dev/vdb"])
        .output()
        .context("run static BusyBox blkid")?;
    ensure!(output.status.success(), "static BusyBox blkid failed");
    let expected = format!("LABEL=\"{label}\"");
    for line in std::str::from_utf8(&output.stdout)?.lines() {
        let Some((device, attributes)) = line.split_once(':') else {
            continue;
        };
        if safe_block_device(device)
            && attributes
                .split_ascii_whitespace()
                .any(|attribute| attribute == expected)
        {
            return Ok(PathBuf::from(device));
        }
    }
    bail!("no block filesystem bears label {label}")
}

fn safe_block_device(path: &str) -> bool {
    path.starts_with("/dev/")
        && path.len() <= 64
        && !path.contains(['\n', '\r', '\0'])
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

fn mount_ext4(device: &Path, target: &Path) -> Result<()> {
    mount(
        Some(device),
        target,
        Some("ext4"),
        libc::MS_NODEV | libc::MS_NOSUID | libc::MS_NOATIME,
        Some("rw"),
    )
    .with_context(|| {
        format!(
            "mount workspace device {} at {}",
            device.display(),
            target.display()
        )
    })
}

fn mount_tmpfs(target: &Path) -> Result<()> {
    mount(
        None,
        target,
        Some("tmpfs"),
        libc::MS_NODEV | libc::MS_NOSUID,
        // Directory traversal must be available to sandboxed package-manager
        // users so they can follow /etc/resolv.conf into this tmpfs. Secret
        // files and the forwarded agent socket remain mode 0600.
        Some("mode=0755,size=16m"),
    )
    .with_context(|| format!("mount session tmpfs at {}", target.display()))
}

fn mount(
    source: Option<&Path>,
    target: &Path,
    fs_type: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let source = source.map(c_path).transpose()?;
    let target = c_path(target)?;
    let fs_type = fs_type.map(CString::new).transpose()?;
    let data = data.map(CString::new).transpose()?;
    // SAFETY: all pointers are valid NUL-terminated strings for the duration
    // of the syscall; null denotes an omitted optional mount argument.
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            fs_type
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("mount syscall");
    }
    Ok(())
}

#[derive(Debug)]
struct MountEntry {
    source: PathBuf,
    fs_type: String,
    options: String,
}

fn exact_mount(path: &Path) -> Result<bool> {
    Ok(mount_entry(path)?.is_some())
}

fn mount_entry(path: &Path) -> Result<Option<MountEntry>> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize mount point {}", path.display()))?;
    let contents =
        fs::read_to_string("/proc/self/mountinfo").context("read /proc/self/mountinfo")?;
    for line in contents.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            bail!("malformed /proc/self/mountinfo line");
        };
        let mut left_fields = left.split_ascii_whitespace();
        let target = left_fields.nth(4).context("mountinfo has no target")?;
        let options = left_fields.next().context("mountinfo has no options")?;
        let mut right_fields = right.split_ascii_whitespace();
        let fs_type = right_fields
            .next()
            .context("mountinfo has no filesystem type")?;
        let source = right_fields
            .next()
            .context("mountinfo has no mount source")?;
        let target = PathBuf::from(unescape_mountinfo(target)?);
        if target == canonical {
            return Ok(Some(MountEntry {
                source: PathBuf::from(unescape_mountinfo(source)?),
                fs_type: fs_type.to_owned(),
                options: options.to_owned(),
            }));
        }
    }
    Ok(None)
}

fn unescape_mountinfo(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            ensure!(index + 3 < bytes.len(), "truncated mountinfo escape");
            let octal = &input[index + 1..index + 4];
            ensure!(
                octal.bytes().all(|byte| matches!(byte, b'0'..=b'7')),
                "invalid mountinfo escape"
            );
            output.push(u8::from_str_radix(octal, 8)?);
            index += 4;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).context("mount point is not UTF-8")
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mountinfo_paths() {
        assert_eq!(unescape_mountinfo("/a\\040b\\134c").unwrap(), "/a b\\c");
        assert!(unescape_mountinfo("/bad\\1").is_err());
    }

    #[test]
    fn root_is_an_exact_mount() {
        assert!(exact_mount(Path::new("/")).unwrap());
    }
}
