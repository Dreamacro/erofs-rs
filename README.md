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
- Full/Compact compression indexes, multi-block pclusters, inline tails, packed fragments, partial references, and 4/8/16/32-byte extent records
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
use erofs_rs::build::{Builder, Metadata};

let mut builder = Builder::new(Cursor::new(Vec::new()))?;
builder.append_dir("etc", Metadata { mode: 0o755, ..Metadata::default() })?;
builder.append_file("etc/config", Metadata::default(), 4, &b"data"[..])?;
builder.append_symlink("config", Metadata { mode: 0o777, ..Metadata::default() }, "etc/config")?;
builder.append_hard_link("config-copy", "etc/config")?;
let image = builder.finish()?.into_inner();
```

- `Builder<W>` requires an empty `Write + Seek` output; a file or `Cursor<Vec<u8>>` works.
- `Metadata` contains permission bits, UID/GID and signed Unix modification time with nanoseconds.
- `append_file(path, metadata, size, reader)` consumes exactly `size` bytes immediately;
  extra bytes remain unread. Full blocks are streamed, while inline tails share one
  pending 4 KiB metadata block. No per-file payload buffer or retained input handle is needed.
- Paths are raw Unix bytes, with an optional leading `/`. Empty components, `.`, `..`,
  NUL, duplicate paths and components above 255 bytes are rejected. Only directories
  allow a trailing `/`. Parents can be added later but must exist by `finish()`.
- The root defaults to mode `0o755`, UID/GID zero and epoch time;
  `append_dir("/", metadata)` sets its metadata once.
- Hard links share an existing file/symlink inode; forward and directory hard links
  are rejected. Symlink targets are preserved verbatim, not resolved.
- `finish()` flushes pending metadata, writes directories, patches hard-link counts
  and writes the final superblock, flushes, and returns
  the output. Dropping does not finish. Append validation errors leave the builder
  usable, but any read/write failure poisons it; discard that incomplete image.

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
shared metadata. Both builders use the same validation, layout and block encoder,
and emit identical bytes for identical ordered entries.
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

Directory import uses
Unix metadata and `rustix` xattr enumeration (Linux, Android, Apple platforms and Hurd).
Other Unix targets currently return `NotSupported` rather than silently omitting
attributes. This does not add Windows support to the library or define Windows-to-Unix
metadata mappings.

For `from_directory`, the output must be a new file outside the source tree. Keep the input tree unchanged
throughout the build; this is not a filesystem snapshot. Files are copied with bounded
buffers, while directory entries and metadata are kept in memory. Permissions, UID/GID,
modification times and raw names/link targets are preserved. Hard-link counts include
only links inside the image.

## Feature Flags

- `std` (default): Enables standard library support, including mmap and basic image building
- `opendal`: Enables async I/O via [Apache OpenDAL](https://opendal.apache.org/), supporting remote backends (HTTP, S3, etc.)
- `tokio`: Adds the `backend::TokioIo` adapter; implies `std`, but does not enable a runtime or filesystem features. `AsyncBuilder` itself only needs `std`
- `lz4`: Enables LZ4 decoding via `lz4_flex`, without requiring `std`
- `lzma`: Enables EROFS MicroLZMA decoding via `lzma-rs`; implies `std`
- `deflate`: Enables raw DEFLATE decoding via `miniz_oxide`, without requiring `std`
- `zstd`: Enables Zstd decoding via `ruzstd`, without requiring `std`
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
- [x] Extended attribute reading (inline, shared, and long prefixes)
- [x] Flat plain layout
- [x] Flat inline layout
- [x] Chunk-based layout, including indexed and 48-bit chunks
- [x] Additional devices (chunk IDs and unified data addresses)
- [x] Metabox metadata, including inode records, indexes, inline data, and xattrs
- [x] LZ4, MicroLZMA, DEFLATE, and Zstd compressed data (layouts and limits above)
- [x] Directory walk (`walk_dir`)
- [x] Convert to tar archive
- [x] Sync/async incremental image building (`Builder` / `AsyncBuilder`, shared encoding) and Unix directory import

### TODO

- [ ] Writer support for xattrs, special files, compact inodes, compression and advanced formats

## Fuzz testing

The independent [`fuzz/` workspace](fuzz/README.md) covers filesystem parsing,
compression, xattrs, devices, and sync/async read contracts with cargo-fuzz.
It includes seed inputs, bounded in-memory backends, and replay/minimization instructions.

## License

MIT OR Apache-2.0
