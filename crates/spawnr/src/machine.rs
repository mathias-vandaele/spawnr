use crate::cli::Command;
use crate::credentials::HostCredentials;
use crate::guest::{GuestClient, terminal_size};
use crate::paths::Paths;
use crate::state::{EnvironmentRecord, MachineRecord, StateStore};
use anyhow::{Context, Result, bail};
use spawnr_protocol::{Request, Response};
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;
use uuid::Uuid;

const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

pub struct Application {
    paths: Paths,
    verbose: u8,
}

impl Application {
    pub fn new(paths: Paths, verbose: u8) -> Self {
        Self { paths, verbose }
    }

    pub fn run(self, command: Command) -> Result<()> {
        match command {
            Command::Init { name, environment } => self.init(&name, &environment),
            Command::Clone {
                environment,
                repository,
                name,
                count,
            } => self.clone_machines(&environment, &repository, name.as_deref(), count),
            Command::Start { name } => self.start(&name),
            Command::Stop { name } => self.stop(&name),
            Command::Open { name } => self.open(&name),
            Command::Publish { name, reference } => self.publish(&name, &reference),
            Command::Ls { json } => self.list(json),
            Command::Rm { name, force } => self.remove(&name, force),
            Command::Doctor { json } => self.doctor(json),
        }
    }

    fn init(&self, name: &str, environment: &str) -> Result<()> {
        validate_name(name)?;
        validate_environment_reference(environment)?;
        let store = StateStore::open(self.paths.clone())?;
        let mut locked = store.lock()?;
        if locked.state.machines.contains_key(name) {
            bail!("a Spawnr machine named \"{name}\" already exists");
        }

        let mut record = new_record(name, environment, None, None, &locked.state.machines)?;
        let base = self.prepare_storage(&mut record)?;
        let ownership_commit = crate::process::TerminationSignalBlock::acquire()
            .context("protect machine ownership commit")?;
        self.commit_storage(&record, &base)?;
        locked
            .state
            .machines
            .insert(name.to_owned(), record.clone());
        if let Err(error) = locked.save() {
            let cleanup = self.rollback_created(&mut locked, &[record]);
            return Err(with_rollback(error, cleanup));
        }
        drop(ownership_commit);
        println!("✓ created {name}");
        println!("\nReady:\n  spawnr start {name}\n  spawnr open {name}");
        Ok(())
    }

    fn clone_machines(
        &self,
        environment: &str,
        repository: &str,
        requested_name: Option<&str>,
        count: u16,
    ) -> Result<()> {
        validate_environment_reference(environment)?;
        validate_repository(repository)?;
        if let Some(name) = requested_name {
            validate_name(name)?;
        }
        let store = StateStore::open(self.paths.clone())?;
        let mut locked = store.lock()?;
        let names = allocate_clone_names(
            requested_name,
            repository,
            count,
            locked.state.machines.keys(),
        )?;
        let repository_dir = repository_directory(repository)?;
        let mut created = Vec::new();

        for name in &names {
            let mut record = new_record(
                name,
                environment,
                Some(repository),
                Some(&repository_dir),
                &locked.state.machines,
            )?;
            let base = match self.prepare_storage(&mut record) {
                Ok(base) => base,
                Err(error) => {
                    let cleanup = self.rollback_created(&mut locked, &created);
                    return Err(with_rollback(
                        error.context(format!("prepare machine \"{name}\"")),
                        cleanup,
                    ));
                }
            };
            let ownership_commit = match crate::process::TerminationSignalBlock::acquire() {
                Ok(commit) => commit,
                Err(error) => {
                    let cleanup = self.rollback_created(&mut locked, &created);
                    return Err(with_rollback(
                        error.context("protect machine ownership commit"),
                        cleanup,
                    ));
                }
            };
            if let Err(error) = self.commit_storage(&record, &base) {
                drop(ownership_commit);
                let cleanup = self.rollback_created(&mut locked, &created);
                return Err(with_rollback(
                    error.context(format!("create machine \"{name}\"")),
                    cleanup,
                ));
            }
            locked.state.machines.insert(name.clone(), record.clone());
            if let Err(error) = locked.save() {
                let current_cleanup = self.remove_storage(&record);
                if current_cleanup.is_ok() {
                    locked.state.machines.remove(name);
                }
                let prior_cleanup = self.rollback_created(&mut locked, &created);
                return Err(with_rollback(error, current_cleanup.and(prior_cleanup)));
            }
            drop(ownership_commit);
            created.push(record.clone());

            if let Err(error) = self.start_record(&record).and_then(|_| {
                self.guest(&record)
                    .request_timeout(
                        &Request::CloneRepository {
                            repository: repository.to_owned(),
                            destination: format!("/workspace/{repository_dir}"),
                        },
                        CLONE_TIMEOUT,
                    )
                    .and_then(expect_ok)
            }) {
                let cleanup = self.rollback_created(&mut locked, &created);
                return Err(with_rollback(
                    error.context(format!("clone {repository} inside machine \"{name}\"")),
                    cleanup,
                ));
            }
            println!("✓ created {name}");
        }

        if names.len() == 1 {
            println!("✓ cloned {repository}");
            println!("\nReady:\n  spawnr open {}", names[0]);
        } else {
            println!(
                "✓ cloned {repository} into {} independent machines",
                names.len()
            );
            println!("\nReady:");
            for name in names {
                println!("  spawnr open {name}");
            }
        }
        Ok(())
    }

    fn start(&self, name: &str) -> Result<()> {
        let store = StateStore::open(self.paths.clone())?;
        let locked = store.lock()?;
        let record = lookup(&locked.state.machines, name)?;
        if self.machine_status(record)? == MachineStatus::Running {
            println!("{name} is already running");
            return Ok(());
        }
        self.start_record(record)?;
        println!("✓ started {name}");
        Ok(())
    }

    fn stop(&self, name: &str) -> Result<()> {
        let store = StateStore::open(self.paths.clone())?;
        let locked = store.lock()?;
        let record = lookup(&locked.state.machines, name)?;
        let was_stopped = self.machine_status(record)? == MachineStatus::Stopped;
        // Always execute host-side cleanup. A crashed VMM can leave a passt
        // identity or stale PID record even though the VM itself is stopped.
        self.stop_record(record)?;
        if was_stopped {
            println!("{name} is already stopped");
            return Ok(());
        }
        println!("✓ stopped {name}");
        Ok(())
    }

    fn open(&self, name: &str) -> Result<()> {
        let store = StateStore::open(self.paths.clone())?;
        let locked = store.lock()?;
        let record = lookup(&locked.state.machines, name)?.clone();
        if self.machine_status(&record)? != MachineStatus::Running {
            self.start_record(&record)?;
        } else {
            self.configure_session(&record)?;
        }
        // State and lifecycle setup are complete. Never retain the global
        // state flock for an unbounded human shell: unrelated commands and a
        // deliberate `spawnr stop` must remain usable while `open` is active.
        drop(locked);
        let (rows, cols) = terminal_size();
        let cwd = record
            .repository_dir
            .as_ref()
            .map(|directory| format!("/workspace/{directory}"))
            .or_else(|| Some("/workspace".into()));
        let env = BTreeMap::from([("HISTFILE".to_owned(), "/run/spawnr/bash-history".to_owned())]);
        let request = Request::Exec {
            argv: vec!["/bin/bash".into(), "-l".into()],
            cwd,
            env,
            tty: true,
            rows,
            cols,
        };
        let code = self.guest(&record).interactive_exec(request)?;
        if code != 0 {
            bail!("interactive shell exited with status {code}");
        }
        Ok(())
    }

    fn publish(&self, name: &str, reference: &str) -> Result<()> {
        validate_environment_reference(reference)?;
        let store = StateStore::open(self.paths.clone())?;
        let locked = store.lock()?;
        let record = lookup(&locked.state.machines, name)?;
        let status = self.machine_status(record)?;
        let was_running = status == MachineStatus::Running;
        if status != MachineStatus::Stopped {
            self.stop_record(record)
                .context("stop the VM to obtain a consistent environment snapshot")?;
        }

        let result = self.publish_environment(record, reference);
        let restart = if was_running {
            self.start_record(record)
                .context("published, but failed to restore the machine's running state")
        } else {
            Ok(())
        };
        match (result, restart) {
            (Ok(()), Ok(())) => {}
            (Err(publish), Ok(())) => return Err(publish),
            (Ok(()), Err(restart)) => {
                return Err(restart).with_context(|| {
                    format!(
                        "environment was published successfully to {reference}, but the VM remained stopped"
                    )
                });
            }
            (Err(publish), Err(restart)) => {
                return Err(publish).with_context(|| {
                    format!(
                        "publishing failed and restoring the running VM also failed: {restart:#}"
                    )
                });
            }
        }
        println!("✓ published environment from {name} to {reference}");
        println!("  workspace and session storage were excluded structurally");
        Ok(())
    }

    fn list(&self, json: bool) -> Result<()> {
        let store = StateStore::open(self.paths.clone())?;
        let locked = store.lock()?;
        let mut rows = Vec::new();
        for record in locked.state.machines.values() {
            rows.push(ListRow {
                name: record.name.clone(),
                environment: record.environment.reference.clone(),
                repository: record.repository.clone(),
                status: self.machine_status(record)?.as_str().to_owned(),
            });
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }
        if rows.is_empty() {
            println!("No Spawnr machines.");
            return Ok(());
        }
        let name_width = rows.iter().map(|row| row.name.len()).max().unwrap().max(4);
        let env_width = rows
            .iter()
            .map(|row| row.environment.len())
            .max()
            .unwrap()
            .max(11);
        let repo_width = rows
            .iter()
            .map(|row| repository_display(row.repository.as_deref()).len())
            .max()
            .unwrap()
            .max(10);
        println!(
            "{:<name_width$}  {:<env_width$}  {:<repo_width$}  STATUS",
            "NAME", "ENVIRONMENT", "REPOSITORY"
        );
        for row in rows {
            println!(
                "{:<name_width$}  {:<env_width$}  {:<repo_width$}  {}",
                row.name,
                row.environment,
                repository_display(row.repository.as_deref()),
                row.status
            );
        }
        Ok(())
    }

    fn remove(&self, name: &str, force: bool) -> Result<()> {
        let store = StateStore::open(self.paths.clone())?;
        let mut locked = store.lock()?;
        let record = lookup(&locked.state.machines, name)?.clone();
        let was_running = self.machine_status(&record)? == MachineStatus::Running;

        if record.repository.is_some() && !force {
            if !was_running {
                self.start_record(&record)
                    .context("start the machine to verify its Git workspace before destruction")?;
            }
            let status = self.guest(&record).request_timeout(
                &Request::WorkspaceStatus {
                    repository_path: format!(
                        "/workspace/{}",
                        record.repository_dir.as_deref().unwrap_or("")
                    ),
                },
                std::time::Duration::from_secs(30),
            );
            if !was_running {
                let _ = self.stop_record(&record);
            }
            match status? {
                Response::WorkspaceStatus {
                    clean: false,
                    porcelain,
                } => bail!(
                    "Workspace contains uncommitted changes:\n\n{}\nUse --force to destroy.",
                    indent(&porcelain)
                ),
                Response::WorkspaceStatus { clean: true, .. } => {}
                Response::Error { message } => bail!(
                    "could not verify workspace cleanliness: {message}\nUse --force only if discarding the workspace is intentional."
                ),
                other => bail!("guest returned an unexpected workspace response: {other:?}"),
            }
        }

        if self.machine_status(&record)? == MachineStatus::Running {
            self.stop_record(&record)?;
        }
        self.remove_storage(&record)?;
        locked.state.machines.remove(name);
        locked.save()?;
        println!("✓ removed {name}");
        println!("  environment and workspace disks were destroyed");
        Ok(())
    }

    fn rollback_created(
        &self,
        locked: &mut crate::state::LockedState<'_>,
        created: &[MachineRecord],
    ) -> Result<()> {
        let mut failures = Vec::new();
        for record in created.iter().rev() {
            match self.remove_storage(record) {
                Ok(()) => {
                    locked.state.machines.remove(&record.name);
                }
                Err(error) => failures.push(format!("{}: {error:#}", record.name)),
            }
        }
        locked.save()?;
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "could not remove {}; their state records were preserved",
                failures.join("; ")
            )
        }
    }

    fn configure_session(&self, record: &MachineRecord) -> Result<()> {
        let credentials = HostCredentials::collect();
        let machine_paths = self.runtime_paths(record);
        if let Some(agent_link) = credentials.expose_ssh_agent(&machine_paths.vsock_socket)? {
            // Cloud Hypervisor follows the symlink each time the guest opens
            // the session-scoped agent connection. Keep it until VM shutdown.
            agent_link.persist();
        }
        expect_ok(self.guest(record).request_timeout(
            &Request::ConfigureSession(credentials.session),
            std::time::Duration::from_secs(5),
        )?)
    }

    fn guest(&self, record: &MachineRecord) -> GuestClient {
        GuestClient::new(self.runtime_paths(record).vsock_socket)
    }

    // The following adapter methods keep lifecycle policy here while Linux,
    // storage, and OCI mechanics remain in focused modules.
    fn prepare_storage(&self, record: &mut MachineRecord) -> Result<std::path::PathBuf> {
        crate::storage::prepare_machine_environment(&self.paths, record, self.verbose)
    }

    fn commit_storage(&self, record: &MachineRecord, base: &std::path::Path) -> Result<()> {
        crate::storage::commit_machine_storage(&self.paths, record, base)
    }

    fn remove_storage(&self, record: &MachineRecord) -> Result<()> {
        crate::storage::remove_machine(&self.paths, record)
    }

    fn runtime_paths(&self, record: &MachineRecord) -> crate::storage::MachinePaths {
        crate::storage::MachinePaths::for_record(&self.paths, record)
    }

    fn machine_status(&self, record: &MachineRecord) -> Result<MachineStatus> {
        Ok(match crate::vmm::status(&self.paths, record)? {
            crate::vmm::VmmStatus::Running => MachineStatus::Running,
            crate::vmm::VmmStatus::Stopped => MachineStatus::Stopped,
            crate::vmm::VmmStatus::Degraded => MachineStatus::Degraded,
        })
    }

    fn start_record(&self, record: &MachineRecord) -> Result<()> {
        let credentials = HostCredentials::collect();
        crate::vmm::start(&self.paths, record, &credentials, self.verbose)?;
        let guest = self.guest(record);
        if let Err(error) = guest.wait_healthy(std::time::Duration::from_secs(30)) {
            let cleanup = crate::vmm::stop(&self.paths, record, self.verbose);
            let error = error.context(format!("start \"{}\"", record.name));
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => error.context(format!(
                    "failed boot cleanup also failed; ownership records were preserved: {cleanup:#}"
                )),
            });
        }
        if let Err(error) = guest
            .request_timeout(
                &Request::ConfigureSession(credentials.session),
                std::time::Duration::from_secs(5),
            )
            .and_then(expect_ok)
        {
            let cleanup = crate::vmm::stop(&self.paths, record, self.verbose);
            return match cleanup {
                Ok(()) => Err(error).context("configure guest session after boot"),
                Err(cleanup) => Err(error).with_context(|| {
                    format!("configure guest session after boot; cleanup also failed: {cleanup:#}")
                }),
            };
        }
        Ok(())
    }

    fn stop_record(&self, record: &MachineRecord) -> Result<()> {
        let guest_shutdown = self
            .guest(record)
            .request_timeout(&Request::Shutdown, std::time::Duration::from_secs(3))
            .and_then(expect_ok);
        match crate::vmm::stop(&self.paths, record, self.verbose) {
            Ok(()) => Ok(()),
            Err(error) => match guest_shutdown {
                Ok(()) => Err(error),
                Err(guest_error) => Err(error)
                    .with_context(|| format!("guest shutdown also failed: {guest_error:#}")),
            },
        }
    }

    fn publish_environment(&self, record: &MachineRecord, reference: &str) -> Result<()> {
        crate::oci::publish_machine_environment(&self.paths, record, reference, self.verbose)
    }

    fn doctor(&self, json: bool) -> Result<()> {
        crate::vmm::doctor(&self.paths, json)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MachineStatus {
    Running,
    Stopped,
    Degraded,
}

impl MachineStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(serde::Serialize)]
struct ListRow {
    name: String,
    environment: String,
    repository: Option<String>,
    status: String,
}

fn expect_ok(response: Response) -> Result<()> {
    match response {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => bail!("guest operation failed: {message}"),
        response => bail!("guest returned an unexpected response: {response:?}"),
    }
}

fn with_rollback(primary: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => primary,
        Err(rollback) => primary.context(format!(
            "automatic rollback also failed; recoverable ownership state was preserved: {rollback:#}"
        )),
    }
}

fn lookup<'a>(
    machines: &'a BTreeMap<String, MachineRecord>,
    name: &str,
) -> Result<&'a MachineRecord> {
    machines
        .get(name)
        .with_context(|| format!("no Spawnr machine named \"{name}\""))
}

fn new_record(
    name: &str,
    environment: &str,
    repository: Option<&str>,
    repository_dir: Option<&str>,
    existing: &BTreeMap<String, MachineRecord>,
) -> Result<MachineRecord> {
    let id = Uuid::new_v4();
    let used_cids = existing
        .values()
        .map(|record| record.vsock_cid)
        .collect::<BTreeSet<_>>();
    let mut cid =
        4_096 + (u32::from_be_bytes(id.as_bytes()[0..4].try_into().unwrap()) & 0x3fff_ffff);
    while used_cids.contains(&cid) {
        cid = cid.checked_add(1).unwrap_or(4_096);
    }
    let bytes = id.as_bytes();
    Ok(MachineRecord {
        id,
        name: name.into(),
        environment: EnvironmentRecord {
            reference: environment.into(),
            manifest_digest: None,
            base_cache_key: None,
        },
        repository: repository.map(Into::into),
        repository_dir: repository_dir.map(Into::into),
        workspace_id: Uuid::new_v4(),
        created_at: OffsetDateTime::now_utc(),
        vsock_cid: cid,
        mac_address: format!(
            "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]
        ),
    })
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 {
        bail!("machine name must contain 1 to 63 characters");
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
        || !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        bail!(
            "invalid machine name \"{name}\": use lowercase letters, digits, and interior hyphens"
        );
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    if repository.is_empty() || repository.contains(['\n', '\r', '\0']) {
        bail!("repository must be a non-empty, single-line Git URL");
    }
    repository_directory(repository).map(|_| ())
}

fn validate_environment_reference(reference: &str) -> Result<()> {
    if reference.is_empty()
        || reference.starts_with('-')
        || reference.chars().any(char::is_whitespace)
        || reference.contains('\0')
    {
        bail!("invalid OCI environment reference \"{reference}\"");
    }
    Ok(())
}

fn repository_directory(repository: &str) -> Result<String> {
    let trimmed = repository.trim_end_matches('/');
    let tail = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .strip_suffix(".git")
        .unwrap_or_else(|| trimmed.rsplit(['/', ':']).next().unwrap_or(""));
    let result = slug(tail);
    if result.is_empty() {
        bail!("cannot derive a workspace directory from repository \"{repository}\"");
    }
    Ok(result)
}

fn allocate_clone_names<'a>(
    requested: Option<&str>,
    repository: &str,
    count: u16,
    existing: impl Iterator<Item = &'a String>,
) -> Result<Vec<String>> {
    let mut occupied = existing.cloned().collect::<BTreeSet<_>>();
    if count == 1
        && let Some(name) = requested
    {
        if occupied.contains(name) {
            bail!("a Spawnr machine named \"{name}\" already exists");
        }
        return Ok(vec![name.to_owned()]);
    }
    let base = requested
        .map(str::to_owned)
        .unwrap_or(repository_directory(repository)?);
    let mut names = Vec::with_capacity(count as usize);
    let mut suffix = 1_u32;
    while names.len() < count as usize {
        let maximum_base = 63_usize.saturating_sub(suffix.to_string().len() + 1);
        let truncated = base.get(..base.len().min(maximum_base)).unwrap_or(&base);
        let candidate = format!("{}-{suffix}", truncated.trim_end_matches('-'));
        if !occupied.contains(&candidate) {
            validate_name(&candidate)?;
            occupied.insert(candidate.clone());
            names.push(candidate);
        }
        suffix += 1;
    }
    Ok(names)
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            separator = false;
            output.push(character);
        } else {
            separator = true;
        }
    }
    output.truncate(output.len().min(50));
    output.trim_end_matches('-').to_owned()
}

fn repository_display(repository: Option<&str>) -> String {
    repository
        .and_then(|repository| {
            let trimmed = repository.trim_end_matches('/').trim_end_matches(".git");
            let parts = trimmed.rsplit(['/', ':']).take(2).collect::<Vec<_>>();
            (parts.len() == 2).then(|| format!("{}/{}", parts[1], parts[0]))
        })
        .unwrap_or_else(|| "-".into())
}

fn indent(value: &str) -> String {
    value
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_machine_names_as_hostnames() {
        for valid in ["foo", "foo-1", "9", "a-b-c"] {
            validate_name(valid).unwrap();
        }
        for invalid in ["", "Foo", "-foo", "foo-", "foo_bar", "a/b"] {
            assert!(validate_name(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn derives_unique_clone_names() {
        let existing = ["project-1".to_owned(), "project-3".to_owned()];
        let names =
            allocate_clone_names(None, "git@github.com:acme/Project.git", 3, existing.iter())
                .unwrap();
        assert_eq!(names, ["project-2", "project-4", "project-5"]);
    }

    #[test]
    fn explicit_single_name_is_not_suffixed() {
        let none: [String; 0] = [];
        assert_eq!(
            allocate_clone_names(Some("work"), "x/y.git", 1, none.iter()).unwrap(),
            ["work"]
        );
    }

    #[test]
    fn displays_git_repository_concisely() {
        assert_eq!(
            repository_display(Some("git@github.com:acme/foo.git")),
            "acme/foo"
        );
    }
}
