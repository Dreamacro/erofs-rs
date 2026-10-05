use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use erofs_rs::build::{Compression, InodeFormat, Options};
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Source directory; its contents must remain unchanged during the build.
    source: PathBuf,
    /// New image path, outside the source directory. Existing files are never replaced.
    #[arg(short, long)]
    output: PathBuf,
    /// Filesystem UUID; defaults to the zero UUID, not a random value.
    #[arg(long, default_value_t = Uuid::nil())]
    uuid: Uuid,
    /// Filesystem creation time in signed Unix seconds; inode mtimes are preserved.
    #[arg(
        long,
        default_value_t = 0,
        allow_negative_numbers = true,
        value_name = "SECONDS"
    )]
    build_time: i64,
    /// Nanosecond part of the filesystem creation time (0..999999999).
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..1_000_000_000), value_name = "NANOSECONDS")]
    build_time_nsec: u32,
    /// Regular-file compression; small files retain flat/inline storage.
    #[arg(long, default_value = "none", value_parser = ["none", "lz4", "lzma", "deflate", "zstd"])]
    compression: String,
    /// Inode headers; compact rejects unrepresentable metadata rather than changing it.
    #[arg(long, default_value = "auto", value_parser = ["auto", "compact", "extended"])]
    inode_format: String,
}

pub fn build(args: BuildArgs) -> Result<()> {
    Options::default()
        .uuid(args.uuid.into_bytes())
        .build_time((args.build_time, args.build_time_nsec))
        .compression(match args.compression.as_str() {
            "lz4" => Compression::Lz4,
            "lzma" => Compression::Lzma,
            "deflate" => Compression::Deflate,
            "zstd" => Compression::Zstd,
            _ => Compression::None,
        })
        .inode_format(match args.inode_format.as_str() {
            "compact" => InodeFormat::Compact,
            "extended" => InodeFormat::Extended,
            _ => InodeFormat::Auto,
        })
        .build_from_directory(args.source, args.output)
        .context("failed to build EROFS image")
}
