use anyhow::{Context, Result, bail};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn discover(override_root: Option<&Path>) -> Result<Self> {
        let root = if let Some(root) = override_root {
            root.to_path_buf()
        } else if let Some(root) = env::var_os("SPAWNR_HOME") {
            PathBuf::from(root)
        } else if let Some(root) = env::var_os("XDG_DATA_HOME") {
            PathBuf::from(root).join("spawnr")
        } else if let Some(home) = env::var_os("HOME") {
            PathBuf::from(home).join(".local/share/spawnr")
        } else {
            bail!("cannot locate a data directory: HOME and XDG_DATA_HOME are unset");
        };

        if !root.is_absolute() {
            bail!("Spawnr data directory must be absolute: {}", root.display());
        }
        Ok(Self { root })
    }

    pub fn ensure_layout(&self) -> Result<()> {
        create_private_dir(&self.root)?;
        for path in [
            self.machines_dir(),
            self.images_dir(),
            self.oci_dir(),
            self.blobs_dir(),
            self.bin_dir(),
        ] {
            create_private_dir(&path)?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join("state.lock")
    }

    pub fn machines_dir(&self) -> PathBuf {
        self.root.join("machines")
    }

    pub fn machine_dir(&self, id: &str) -> PathBuf {
        self.machines_dir().join(id)
    }

    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn oci_dir(&self) -> PathBuf {
        self.root.join("oci")
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs/sha256")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
}

pub fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create private directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure directory {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_root_must_be_absolute() {
        let error = Paths::discover(Some(Path::new("relative"))).unwrap_err();
        assert!(error.to_string().contains("absolute"));
    }
}
