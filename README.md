# erofs-rs

A pure Rust library for reading and building [EROFS](https://docs.kernel.org/filesystems/erofs.html) (Enhanced Read-Only File System) images.

> **Note**: This library aims to provide essential parsing and building capabilities for common use cases, not a full reimplementation of [erofs-utils](https://github.com/erofs/erofs-utils).

## Features

- **no_std support** with `alloc` on supported targets
- Zero-copy parsing via mmap (std) or byte slices (no_std)
- Directory traversal and file reading
- Inline/shared extended attributes and long name prefixes, with lossless byte names and values
- Multiple data layouts: flat plain, flat inline, chunk-based (including indexed and 48-bit chunks)
- Additional devices, with explicit chunk device IDs and unified-address routing for file data
- Optional LZ4, MicroLZMA, DEFLATE, and Zstd decoding with shared sync/async mapping and per-file decoded-extent caching
- Full/Compact compression indexes, multi-block pclusters, inline tails, packed fragments, partial references, and 4/8/16/32-byte extent records
- Non-default logical cluster sizes and legacy LZ4 trailing padding

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

## Feature Flags

- `std` (default): Enables standard library support, including mmap backend
- `opendal`: Enables async I/O via [Apache OpenDAL](https://opendal.apache.org/), supporting remote backends (HTTP, S3, etc.)
- `lz4`: Enables LZ4 decoding via `lz4_flex`, without requiring `std`
- `lzma`: Enables EROFS MicroLZMA decoding via `lzma-rs`; implies `std`
- `deflate`: Enables raw DEFLATE decoding via `miniz_oxide`, without requiring `std`
- `zstd`: Enables Zstd decoding via `ruzstd`, without requiring `std`
- All four codec features are enabled in the CLI
- Without `std`: Operates in `no_std` mode with `alloc`

```toml
# Standard usage (default)
[dependencies]
erofs-rs = "0.1"

# Async with OpenDAL
[dependencies]
erofs-rs = { version = "0.1", features = ["opendal"] }

# no_std with alloc
[dependencies]
erofs-rs = { version = "0.1", default-features = false }
```

## CLI

Local images, including additional devices, are memory-mapped. Do not modify or truncate them while a command is running.
Repeat `--device` in device-table order; `dump` and `inspect` accept all-local paths or all-HTTP URLs, while `convert` accepts local paths only.

```bash
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
- [x] LZ4, MicroLZMA, DEFLATE, and Zstd compressed data (layouts and limits above)
- [x] Directory walk (`walk_dir`)
- [x] Convert to tar archive

### TODO

- [ ] Metabox metadata
- [ ] Image building (`mkfs.erofs` equivalent)

## License

MIT OR Apache-2.0
