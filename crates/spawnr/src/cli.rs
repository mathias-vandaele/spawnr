use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "spawnr",
    version,
    about = "Spawn isolated, reproducible development computers",
    propagate_version = true
)]
pub struct Cli {
    /// Override Spawnr's local data directory.
    #[arg(long, env = "SPAWNR_HOME", global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Show runtime subprocess diagnostics.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Download, verify, and install Spawnr's versioned runtime.
    Setup {
        /// Use a runtime lock from disk instead of the one embedded in the release CLI.
        #[arg(long, value_name = "PATH")]
        runtime_lock: Option<PathBuf>,

        /// Install a local runtime archive instead of downloading it.
        #[arg(long, value_name = "PATH")]
        runtime_archive: Option<PathBuf>,
    },

    /// Create a development computer without a repository.
    Init {
        name: String,

        /// OCI environment to instantiate.
        #[arg(long, default_value = "docker.io/library/ubuntu:24.04")]
        environment: String,
    },

    /// Instantiate an OCI environment and clone a repository inside it.
    Clone {
        environment: String,
        repository: String,

        #[arg(long)]
        name: Option<String>,

        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=100))]
        count: u16,
    },

    /// Boot a stopped development computer.
    Start { name: String },

    /// Gracefully stop a running development computer.
    Stop { name: String },

    /// Open an interactive shell, starting the computer when necessary.
    Open { name: String },

    /// Publish only the machine's environment as an OCI image.
    Publish { name: String, reference: String },

    /// List Spawnr-owned development computers.
    Ls {
        #[arg(long)]
        json: bool,
    },

    /// Destroy a development computer owned by Spawnr.
    Rm {
        name: String,

        /// Discard uncommitted workspace changes.
        #[arg(long)]
        force: bool,
    },

    /// Verify host capabilities and the installed Spawnr runtime.
    Doctor {
        #[arg(long)]
        json: bool,
    },
}
