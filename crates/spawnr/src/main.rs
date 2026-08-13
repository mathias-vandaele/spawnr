use clap::Parser;
use spawnr::{Application, cli::Cli, paths::Paths};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover(cli.data_dir.as_deref())?;
    Application::new(paths, cli.verbose).run(cli.command)
}
