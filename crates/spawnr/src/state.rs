use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use time::OffsetDateTime;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvironmentRecord {
    pub reference: String,
    pub manifest_digest: Option<String>,
    /// Relative content-addressed cache directory containing the pristine disk.
    pub base_cache_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineRecord {
    pub id: Uuid,
    pub name: String,
    pub environment: EnvironmentRecord,
    pub repository: Option<String>,
    pub repository_dir: Option<String>,
    pub workspace_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub vsock_cid: u32,
    pub mac_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub schema_version: u32,
    pub machines: BTreeMap<String, MachineRecord>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            machines: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnershipMarker {
    pub owner: String,
    pub machine_id: Uuid,
    pub machine_name: String,
}

pub struct StateStore {
    paths: Paths,
}

pub struct LockedState<'a> {
    store: &'a StateStore,
    _lock: File,
    pub state: State,
}

impl StateStore {
    pub fn open(paths: Paths) -> Result<Self> {
        paths.ensure_layout()?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn lock(&self) -> Result<LockedState<'_>> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.paths.lock_file())
            .context("open Spawnr state lock")?;
        lock_exclusive(&lock).context("lock Spawnr state")?;
        let state = self.read()?;
        Ok(LockedState {
            store: self,
            _lock: lock,
            state,
        })
    }

    fn read(&self) -> Result<State> {
        let path = self.paths.state_file();
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(State::default());
            }
            Err(error) => return Err(error).context("open Spawnr state"),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).context("read Spawnr state")?;
        let state: State = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "Spawnr state is corrupt at {} (the file was not modified)",
                path.display()
            )
        })?;
        if state.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported Spawnr state schema {} (this build supports {})",
                state.schema_version,
                SCHEMA_VERSION
            );
        }
        Ok(state)
    }
}

fn lock_exclusive(file: &File) -> std::io::Result<()> {
    // SAFETY: flock only operates on the valid open file descriptor.
    let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

impl LockedState<'_> {
    pub fn save(&self) -> Result<()> {
        let destination = self.store.paths.state_file();
        let temporary = temporary_state_path(&self.store.paths);
        let bytes = serde_json::to_vec_pretty(&self.state).context("encode Spawnr state")?;
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .context("create temporary Spawnr state")?;
            file.write_all(&bytes).context("write Spawnr state")?;
            file.write_all(b"\n").context("finish Spawnr state")?;
            file.sync_all().context("sync Spawnr state")?;
            fs::rename(&temporary, &destination).context("atomically replace Spawnr state")?;
            File::open(self.store.paths.root())
                .and_then(|directory| directory.sync_all())
                .context("sync Spawnr state directory")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn temporary_state_path(paths: &Paths) -> PathBuf {
    paths.root().join(format!(
        ".state.json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("spawnr-state-test-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn state_round_trips_atomically() {
        let temporary = tempdir();
        let paths = Paths::discover(Some(&temporary)).unwrap();
        let store = StateStore::open(paths).unwrap();
        {
            let mut locked = store.lock().unwrap();
            locked.state.machines.insert(
                "demo".into(),
                MachineRecord {
                    id: Uuid::nil(),
                    name: "demo".into(),
                    environment: EnvironmentRecord {
                        reference: "ubuntu".into(),
                        manifest_digest: None,
                        base_cache_key: None,
                    },
                    repository: None,
                    repository_dir: None,
                    workspace_id: Uuid::nil(),
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    vsock_cid: 4_096,
                    mac_address: "02:00:00:00:00:01".into(),
                },
            );
            locked.save().unwrap();
        }
        let locked = store.lock().unwrap();
        assert_eq!(locked.state.machines["demo"].name, "demo");
        fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn corrupt_state_is_never_replaced() {
        let temporary = tempdir();
        let paths = Paths::discover(Some(&temporary)).unwrap();
        paths.ensure_layout().unwrap();
        fs::write(paths.state_file(), b"not json").unwrap();
        let store = StateStore::open(paths.clone()).unwrap();
        assert!(store.lock().is_err());
        assert_eq!(fs::read(paths.state_file()).unwrap(), b"not json");
        fs::remove_dir_all(temporary).unwrap();
    }
}
