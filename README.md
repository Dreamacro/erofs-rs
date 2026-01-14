# erofs-rs

A pure Rust library for reading and building [EROFS](https://docs.kernel.org/filesystems/erofs.html) (Enhanced Read-Only File System) images.

> **Note**: This library aims to provide essential parsing and building capabilities for common use cases, not a full reimplementation of [erofs-utils](https://github.com/erofs/erofs-utils).

## Features

- Zero-copy parsing via mmap
- Directory traversal and file reading
- Multiple data layouts: flat plain, flat inline, chunk-based

## Usage

```rust
use std::fs::File;
use std::io::Read;
use memmap2::Mmap;
use erofs_rs::EroFS;

fn main() -> erofs_rs::Result<()> {
    let file = File::open("system.erofs")?;
    let mmap = unsafe { Mmap::map(&file) }?;
    let fs = EroFS::new(mmap)?;

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
