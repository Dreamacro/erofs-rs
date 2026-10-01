# erofs-rs

A pure Rust library for reading and building [EROFS](https://docs.kernel.org/filesystems/erofs.html) (Enhanced Read-Only File System) images.

> **Note**: This library aims to provide essential parsing and building capabilities for common use cases, not a full reimplementation of [erofs-utils](https://github.com/erofs/erofs-utils).

## Features

- **no_std support** with `alloc` for embedded systems
- Zero-copy parsing via mmap (std) or byte slices (no_std)
- Directory traversal and file reading
- Multiple data layouts: flat plain, flat inline, chunk-based
- Optional LZ4, MicroLZMA, DEFLATE, and Zstd decoding with shared sync/async mapping and per-file decoded-extent caching

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

Local images are memory-mapped. Do not modify or truncate them while a command is running.

```bash
# Dump superblock info
erofs-cli dump image.erofs

# List directory
erofs-cli inspect -i image.erofs ls /

# Read file content
erofs-cli inspect -i image.erofs cat /etc/passwd

# Convert to tar
erofs-cli convert image.erofs -o out.tar

# Remote images via HTTP (async OpenDAL backend)
erofs-cli dump http://example.com/images/system.erofs
erofs-cli inspect -i http://example.com/images/system.erofs ls /
erofs-cli inspect -i http://example.com/images/system.erofs cat /etc/os-release
```

## Status

### Implemented

- [x] Superblock / inode / dirent parsing
- [x] Flat plain layout
- [x] Flat inline layout
- [x] Chunk-based layout (without chunk indexes)
- [x] LZ4, MicroLZMA, DEFLATE, and Zstd compressed data (subset described above)
- [x] Directory walk (`walk_dir`)
- [x] Convert to tar archive

### TODO

- [ ] Extended attributes
- [ ] Remaining compression layouts
- [ ] Image building (`mkfs.erofs` equivalent)

## License

MIT OR Apache-2.0
