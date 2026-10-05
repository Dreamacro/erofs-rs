# erofs-rs

A pure Rust library for reading and building [EROFS](https://docs.kernel.org/filesystems/erofs.html) (Enhanced Read-Only File System) images.

> **Note**: This library aims to provide essential parsing and building capabilities for common use cases, not a full reimplementation of [erofs-utils](https://github.com/erofs/erofs-utils).

## Features

- **no_std support** with `alloc` on supported targets
- Zero-copy parsing via mmap (std) or byte slices (no_std)
- Directory traversal, sequential/positioned file reads, and lossless symlink targets (sync and async)
- Inline/shared extended attributes and long name prefixes, with lossless byte names and values
- Multiple data layouts: flat plain, flat inline, chunk-based (including indexed and 48-bit chunks)
- Additional devices, with explicit chunk device IDs and unified-address routing for file data
- Metabox metadata, decoded on demand through the existing codecs without buffering the entire metadata file
- Optional LZ4, MicroLZMA, DEFLATE, and Zstd decoding with shared sync/async mapping and per-file decoded-extent caching
- Compressed reading with Full/Compact indexes, multi-block pclusters, inline tails, packed fragments, partial references, and 4/8/16/32-byte extent records
- Non-default logical cluster sizes and legacy LZ4 trailing padding
- Incremental image building: shared sync/async layout and encoding, explicit metadata and streaming inputs, plus Unix directory import including Linux and macOS

## Usage

### Standard (with std)

```rust
use std::io::Read;
use erofs_rs::{EroFS, backend::MmapImage};

fn main() -> erofs_rs::Result<()> {
    // SAFETY: ensure no process modifies or truncates the image while it is mapped.
    let image = unsafe { MmapImage::new_from_path("system.erofs")? };
    let fs = EroFS::new(image)?;

    // Read file
    let mut file = fs.open("/etc/os-release")?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    // List directory
    for entry in fs.read_dir("/usr/bin")? {
        println!("{}", String::from_utf8_lossy(entry?.dir_entry.file_name()));
    }

    Ok(())
}
```

### no_std (with alloc)

```rust
#![no_std]

extern crate alloc;
use erofs_rs::{EroFS, backend::SliceImage};

fn main() -> erofs_rs::Result<()> {
    // Assuming you have the EROFS image data in memory
    let image_data: &'static [u8] = include_bytes!("system.erofs");
    let fs = EroFS::new(SliceImage::new(image_data))?;

    // List directory entries
    for entry in fs.read_dir("/etc")? {
        let entry = entry?;
        // Process directory entry...
    }

    // Walk directory tree
    for entry in fs.walk_dir("/")? {
        let entry = entry?;
        // Process each file/directory...
    }

    Ok(())
}
```

### Building images (std)

The low-level API accepts application data without consulting the host filesystem:

```rust
use std::io::Cursor;
use erofs_rs::build::{Metadata, Options, SpecialFile};

let mut builder = Options::default()
    .uuid([0x11; 16])
    .build_time((1_700_000_000, 123_456_789))
    .build(Cursor::new(Vec::new()))?;
builder.append_dir("etc", Metadata { mode: 0o755, ..Metadata::default() })?;
let metadata = Metadata {
    modified: (1_700_000_000, 123_456_789),
    xattrs: [(b"user.comment".to_vec(), b"generated".to_vec())].into(),
    ..Metadata::default()
};
builder.append_file("etc/config", &metadata, 4, &b"data"[..])?;
builder.append_symlink("config", Metadata { mode: 0o777, ..Metadata::default() }, "etc/config")?;
builder.append_hard_link("config-copy", "etc/config")?;
builder.append_special("events", Metadata::default(), SpecialFile::Fifo)?;
let image = builder.finish()?.into_inner();
```

`AsyncBuilder` is also available with `std`, without a runtime dependency:

```rust
use std::io::Cursor;
use erofs_rs::build::{AsyncBuilder, Metadata};

let mut builder = AsyncBuilder::new(Cursor::new(Vec::new())).await?;
builder.append_dir("etc", Metadata { mode: 0o755, ..Metadata::default() })?;
builder.append_file("etc/config", Metadata::default(), 4, &b"data"[..]).await?;
builder.append_symlink("config", Metadata { mode: 0o777, ..Metadata::default() }, "etc/config").await?;
builder.append_hard_link("config-copy", "etc/config")?;
let image = builder.finish().await?.into_inner();
```

It accepts the library's `backend::AsyncRead` inputs and
`backend::AsyncWrite + AsyncSeek` outputs. These use `Send` futures, like the
existing `AsyncImage` backend, without runtime polling traits or an `Unpin`
requirement. Byte slices and memory cursors work directly, as do existing EROFS
async file handles. Borrowed backends (`&mut T`) are supported too. Arbitrary
blocking `std::io` types are deliberately not adapted automatically.

Only I/O operations need `.await`; directory and hard-link additions only update
shared metadata. `AsyncBuilder::append_special` needs `.await` because it can flush
an earlier metadata block, even though special files have no payload. Both builders
use the same validation, layout and block encoder, and emit identical bytes for
identical ordered entries.
An append canceled in its I/O phase poisons the builder just like an I/O error;
its output must be discarded. Dropping an unpolled append future has no effect.
Canceling `new` or `finish` also requires discarding the incomplete output.
`finish()` flushes and returns the writer without shutting it down.
Both async reading and building remain independent of Tokio.

The optional `tokio` feature provides a thin `backend::TokioIo` adapter for owned
or borrowed Tokio types:

```rust
use erofs_rs::{backend::TokioIo, build::{AsyncBuilder, Metadata}};

// `output` and `input` are caller-provided Tokio I/O objects.
let mut builder = AsyncBuilder::new(TokioIo::new(output)).await?;
builder.append_file("file", Metadata::default(), size, TokioIo::new(&mut input)).await?;
let output = builder.finish().await?.into_inner();
```

The synchronous directory helper uses the same builder (run it on a blocking
thread when calling from an async application):

```rust
erofs_rs::build::from_directory("rootfs", "image.erofs")?;
```

Use `Options::default().uuid(...).build_time(...).build_from_directory(source, destination)`
to set the same UUID and creation time. Identical ordered entries (or an unchanged source directory) and
options produce identical image bytes; inode metadata is preserved, not normalized.

Directory import uses
Unix metadata and `rustix` xattr enumeration (Linux, Android, Apple platforms and Hurd).
Other Unix targets currently return `NotSupported` rather than silently omitting
attributes. This does not add Windows support to the library or define Windows-to-Unix
metadata mappings.

For `from_directory`, the output must be a new file outside the source tree. Keep the input tree unchanged
throughout the build; this is not a filesystem snapshot. Files are copied with bounded
buffers, while directory entries and metadata are kept in memory. Permissions, UID/GID,
modification times and raw names/link targets are preserved. FIFOs and character/block
devices are imported as metadata only, preserving device numbers without opening them.
Reported xattrs are read without following symlinks and stored inline. Unsupported
namespaces, format limits and attribute read errors fail the build rather than
silently losing metadata. Sockets remain explicitly rejected. Hard-link counts include only links
inside the image.

## Feature Flags

- `std` (default): Enables standard library support, including mmap and basic image building
- `opendal`: Enables async I/O via [Apache OpenDAL](https://opendal.apache.org/), supporting remote backends (HTTP, S3, etc.)
- `tokio`: Adds the `backend::TokioIo` adapter; implies `std`, but does not enable a runtime or filesystem features. `AsyncBuilder` itself only needs `std`
- `lz4`: Enables LZ4 decoding via `lz4_flex` without requiring `std`; with `std`, also enables LZ4 image writing
- `lzma`: Enables EROFS MicroLZMA reading and writing via `lzma-rust2`; implies `std`
- `deflate`: Enables raw DEFLATE decoding via `miniz_oxide`, without requiring `std`; with `std`, also enables writing
- `zstd`: Enables Zstd decoding via `ruzstd`, without requiring `std`; with `std`, also enables writing
- All four codec features are enabled in the CLI
- Without `std`: Operates in `no_std` mode with `alloc`

```toml
# Standard usage (default)
[dependencies]
erofs-rs = "0.3"

# Async with OpenDAL
[dependencies]
erofs-rs = { version = "0.3", features = ["opendal"] }

# Optional Tokio I/O adapter (async building itself only needs std)
[dependencies]
erofs-rs = { version = "0.3", features = ["tokio"] }

# no_std with alloc
[dependencies]
erofs-rs = { version = "0.3", default-features = false }
```

## CLI

Local images, including additional devices, are memory-mapped. Do not modify or truncate them while a command is running.
Repeat `--device` in device-table order; `dump` and `inspect` accept all-local paths or all-HTTP URLs, while `convert` accepts local paths only.

```bash
# Build an uncompressed image (e.g. Linux/macOS; output must be new and outside rootfs)
erofs-cli build rootfs -o image.erofs

# Compress regular files with Full indexes (none|lz4|lzma|deflate|zstd; default: none)
erofs-cli build rootfs -o compressed.erofs --compression zstd

# Set a fixed UUID and creation time, without changing inode modification times
erofs-cli build rootfs -o configured.erofs \
  --uuid 00112233-4455-6677-8899-aabbccddeeff \
  --build-time 1700000000 --build-time-nsec 123456789

# Dump superblock info
erofs-cli dump image.erofs

# List directory
erofs-cli inspect -i image.erofs ls /

# Read file content
erofs-cli inspect -i image.erofs cat /etc/passwd

# Inspect extended attributes (escaped byte names and values)
erofs-cli inspect -i image.erofs xattrs /etc/os-release

# Convert to tar
erofs-cli convert image.erofs -o out.tar

# Multi-device image (also supported by dump and convert)
erofs-cli inspect -i image.erofs --device data.blob cat /etc/passwd

# Remote images via HTTP (async OpenDAL backend)
erofs-cli dump http://example.com/images/system.erofs
erofs-cli inspect -i http://example.com/images/system.erofs ls /
erofs-cli inspect -i http://example.com/images/system.erofs cat /etc/os-release
```

## Status

### Implemented

- [x] Superblock / inode / dirent parsing
- [x] Flat plain / inline reading
- [x] Chunk-based reading (including indexed/48-bit chunks) and additional devices (chunk IDs/unified addresses)
- [x] Metabox metadata reading (inodes, indexes, inline data and xattrs)
- [x] Compressed reading: LZ4, MicroLZMA, DEFLATE and Zstd (layouts and limits above)
- [x] Xattrs: inline/shared/long-prefix reading; inline writing/import, including cross-block bodies
- [x] Directory traversal (`walk_dir`) and tar export
- [x] Sync/async incremental builders (`Builder` / `AsyncBuilder`) and Unix directory import,
  with compact/extended inodes, optional LZ4/MicroLZMA/DEFLATE/Zstd Full-index compression, hard links,
  FIFOs and character/block devices
- [x] Image UUID/creation-time configuration and superblock CRC32C generation (reader verification pending)

### TODO

- [ ] Writer support for advanced formats

## Fuzz testing

The independent [`fuzz/` workspace](fuzz/README.md) covers filesystem parsing,
compression, xattrs, devices, and sync/async read contracts with cargo-fuzz.
It includes seed inputs, bounded in-memory backends, and replay/minimization instructions.

## License

MIT OR Apache-2.0
