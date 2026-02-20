# erofs-rs

A pure Rust library for reading and building [EROFS](https://docs.kernel.org/filesystems/erofs.html) (Enhanced Read-Only File System) images.

> **Note**: This library aims to provide essential parsing and building capabilities for common use cases, not a full reimplementation of [erofs-utils](https://github.com/erofs/erofs-utils).

## Features

- **no_std support** with `alloc` for embedded systems
- Zero-copy parsing via mmap (std) or byte slices (no_std)
- Directory traversal and file reading
- Multiple data layouts: flat plain, flat inline, chunk-based

## Usage

### Standard (with std)

```rust
use std::io::Read;
use erofs_rs::{EroFS, backend::MmapImage};

fn main() -> erofs_rs::Result<()> {
    let image = MmapImage::new_from_path("system.erofs")?;
    let fs = EroFS::new(image.into())?;

    // Read file
    let mut file = fs.open("/etc/os-release")?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    // List directory
    for entry in fs.read_dir("/usr/bin")? {
        println!("{}", entry?.dir_entry.file_name());
    }

    Ok(())
}
```

### no_std (with alloc)

```rust
#![no_std]

extern crate alloc;
use erofs_rs::{EroFS, backend::{Backend, SliceImage}};

fn main() -> erofs_rs::Result<()> {
    // Assuming you have the EROFS image data in memory
    let image_data: &'static [u8] = include_bytes!("system.erofs");
    let fs = EroFS::new(Backend::Slice(SliceImage::new(image_data)))?;

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
- Without `std`: Operates in `no_std` mode with `alloc`, using slice backend

```toml
# Standard usage (default)
[dependencies]
erofs-rs = "0.1"

# no_std with alloc
[dependencies]
erofs-rs = { version = "0.1", default-features = false }
```

## CLI

```bash
# Dump superblock info
erofs-cli dump image.erofs

# List directory
erofs-cli inspect -i image.erofs ls /

# Read file content
erofs-cli inspect -i image.erofs cat /etc/passwd

# Convert to tar
erofs-cli convert image.erofs -o out.tar
```

## Status

### Implemented

- [x] Superblock / inode / dirent parsing
- [x] Flat plain layout
- [x] Flat inline layout
- [x] Chunk-based layout (without chunk indexes)
- [x] Directory walk (`walk_dir`)
- [x] Convert to tar archive

### TODO

- [ ] Extended attributes
- [ ] Compressed data (lz4, lzma, deflate)
- [ ] Image building (`mkfs.erofs` equivalent)

## License

MIT OR Apache-2.0
