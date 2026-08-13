//! PID 1 orphan reaping without stealing exit status from managed commands.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

static TRACKED_CHILDREN: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

fn tracked() -> &'static Mutex<HashSet<u32>> {
    TRACKED_CHILDREN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Spawn and register atomically with respect to the orphan scanner.
pub fn spawn(command: &mut Command) -> std::io::Result<(Child, ChildGuard)> {
    let mut children = tracked()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let child = command.spawn()?;
    let pid = child.id();
    children.insert(pid);
    Ok((child, ChildGuard { pid }))
}

pub struct ChildGuard {
    pid: u32,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        tracked()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.pid);
    }
}

pub fn start() -> Result<()> {
    thread::Builder::new()
        .name("spawnr-orphan-reaper".into())
        .spawn(|| {
            loop {
                reap_adopted_children();
                thread::sleep(Duration::from_secs(1));
            }
        })
        .context("start PID 1 orphan reaper")?;
    Ok(())
}

fn reap_adopted_children() {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1
            || tracked()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&pid)
        {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if parent_pid(&stat) != Some(1) {
            continue;
        }
        let mut status = 0;
        // SAFETY: this asks only about the exact adopted child discovered in
        // procfs and never blocks. ESRCH/ECHILD races are expected and benign.
        unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    }
}

fn parent_pid(stat: &str) -> Option<u32> {
    let close = stat.rfind(')')?;
    // First field after comm is state (field 3), then parent PID (field 4).
    stat[close + 1..]
        .split_ascii_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_parent_after_pathological_command_name() {
        assert_eq!(parent_pid("42 (odd ) name) S 123 0 0"), Some(123));
    }
}
