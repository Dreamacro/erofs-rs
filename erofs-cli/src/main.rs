use anyhow::Result;
use clap::{Parser, Subcommand};
use jiff::{Timestamp, ZonedDateTime, tz::TimeZone};

#[cfg(unix)]
mod build;
mod convert;
mod dump;
mod inspect;
mod source;

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build an image from a local directory, optionally using compression.
    #[cfg(unix)]
    Build(build::BuildArgs),
    Dump(dump::DumpArgs),
    Inspect(inspect::InspectArgs),
    Convert(convert::ConvertArgs),
}

#[derive(Debug, Parser)]
struct Opt {
    #[command(subcommand)]
    command: Commands,
}

fn local_time((seconds, nanos): (i64, u32)) -> Option<ZonedDateTime> {
    let timestamp = Timestamp::new(seconds, nanos.try_into().ok()?).ok()?;
    Some(timestamp.to_zoned_datetime(TimeZone::system()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();

    match opt.command {
        #[cfg(unix)]
        Commands::Build(args) => build::build(args),
        Commands::Dump(args) => dump::dump(args).await,
        Commands::Inspect(args) => inspect::inspect(args).await,
        Commands::Convert(args) => convert::convert(args),
    }
}
