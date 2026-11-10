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
    /// Maximum physical compression cluster in bytes (4096..=1048576, multiple of 4096).
    #[arg(long, default_value_t = 4096)]
    pcluster_size: u32,
    /// Store final compression extents in unused index-page space.
    #[arg(long)]
    inline_compressed_tails: bool,
    /// Pack remaining short compression tails into a hidden file.
    #[arg(long)]
    fragments: bool,
    /// Reuse identical xattr records through a bounded exact-match cache.
    #[arg(long)]
    shared_xattrs: bool,
    /// Deduplicate long xattr name prefixes (up to 128 prefixes).
    #[arg(long)]
    xattr_prefixes: bool,
    /// Fail on host xattrs outside EROFS namespaces instead of skipping them.
    #[arg(long)]
    reject_unsupported_xattrs: bool,
    /// Compression threads for short files; the image is identical for any count.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    workers: u16,
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
        .pcluster_size(args.pcluster_size)
        .inline_compressed_tails(args.inline_compressed_tails)
        .fragments(args.fragments)
        .shared_xattrs(args.shared_xattrs)
        .xattr_prefixes(args.xattr_prefixes)
        .skip_unsupported_xattrs(!args.reject_unsupported_xattrs)
        .workers(args.workers.into())
        .build_from_directory(args.source, args.output)
        .context("failed to build EROFS image")
}
