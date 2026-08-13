use anyhow::{Context, Result, bail};
use spawnr::runtime::{RuntimeLock, RuntimeManifest};
use std::{env, fs};

fn main() -> Result<()> {
    let mut arguments = env::args_os().skip(1);
    let kind = arguments
        .next()
        .context("usage: validate-runtime <manifest|lock> <file>")?;
    let path = arguments
        .next()
        .context("usage: validate-runtime <manifest|lock> <file>")?;
    if arguments.next().is_some() {
        bail!("usage: validate-runtime <manifest|lock> <file>");
    }

    let contents = fs::read(&path)
        .with_context(|| format!("read runtime contract file {}", path.to_string_lossy()))?;
    match kind.to_string_lossy().as_ref() {
        "manifest" => RuntimeManifest::from_json(&contents).map(|_| ()),
        "lock" => RuntimeLock::from_json(&contents).map(|_| ()),
        _ => bail!("contract kind must be manifest or lock"),
    }
}
