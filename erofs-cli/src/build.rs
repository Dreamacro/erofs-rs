use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Source directory; its contents must remain unchanged during the build.
    source: PathBuf,
    /// New image path, outside the source directory. Existing files are never replaced.
    #[arg(short, long)]
    output: PathBuf,
}

pub fn build(args: BuildArgs) -> Result<()> {
    erofs_rs::build::from_directory(args.source, args.output).context("failed to build EROFS image")
}
