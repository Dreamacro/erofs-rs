use alloc::{format, string::ToString, vec::Vec};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, Metadata as HostMetadata, OpenOptions},
    io::{self, Read, Seek, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use rustix::fs::OFlags;
use typed_path::UnixPath;

use super::{Builder, Metadata};
use crate::{Error, Result};

pub(super) struct Source {
    path: PathBuf,
    metadata: HostMetadata,
}

/// Creates a new EROFS image from a local directory using [`Builder`].
///
/// Directory import is available on Linux, Android, Apple platforms and Hurd.
/// Other Unix platforms currently return `NotSupported`: missing xattr enumeration
/// must not silently discard attributes. The low-level builder needs no host metadata.
///
/// The destination must not exist and must be outside the source directory.
/// File data is streamed; only paths and metadata are kept in memory. Permissions,
/// UID/GID, modification times and byte names/targets are preserved. Hard-link counts
/// reflect links inside the image, not links outside the source tree.
///
/// The source tree must remain stable throughout the build. Detectable changes
/// cause an error, but this is not a snapshot or secure traversal of a concurrently
/// mutable tree. The root path is resolved once; symlinks within it are not followed.
/// Unsupported types and reported xattrs are rejected rather than discarded.
/// Failed builds remove their incomplete output, reporting cleanup failures as well.
///
/// ```no_run
/// # #[cfg(all(feature = "std", unix))]
/// # {
/// erofs_rs::build::from_directory("rootfs", "image.erofs")?;
/// # }
/// # Ok::<(), erofs_rs::Error>(())
/// ```
pub fn from_directory(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
    let source = fs::canonicalize(source)?;
    let destination = destination.as_ref();
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid image output path"))?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let destination = fs::canonicalize(parent)?.join(name);
    if destination.starts_with(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image output must be outside the source directory",
        )
        .into());
    }
    let sources = scan(&source)?;
    let result = {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        write_image(&sources, &mut output).and_then(|()| output.sync_all().map_err(Error::from))
    };
    if let Err(error) = result {
        if let Err(cleanup) = fs::remove_file(&destination) {
            return Err(io::Error::other(format!(
                "{error}; failed to remove incomplete image {}: {cleanup}",
                destination.display()
            ))
            .into());
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn scan(root: &Path) -> Result<Vec<Source>> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Err(Error::NotADirectory(root.display().to_string()));
    }
    let mut directories = HashSet::from([(metadata.dev(), metadata.ino())]);
    let mut sources = vec![new_source(root.to_path_buf(), metadata)?];
    // Iterate the growing list instead of recursing on host directory depth.
    let mut index = 0;
    while index < sources.len() {
        if sources[index].metadata.is_dir() {
            let mut children =
                fs::read_dir(&sources[index].path)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_cached_key(|child| child.file_name());
            for child in children {
                let metadata = fs::symlink_metadata(child.path())?;
                if metadata.is_dir() && !directories.insert((metadata.dev(), metadata.ino())) {
                    return Err(Error::NotSupported(format!(
                        "directory cycle or alias at {}",
                        child.path().display()
                    )));
                }
                sources.push(new_source(child.path(), metadata)?);
            }
        }
        index += 1;
    }
    Ok(sources)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "hurd"
))]
fn reject_xattrs(path: &Path) -> Result<()> {
    if rustix::fs::llistxattr(path, &mut [0u8; 0]).map_err(io::Error::from)? != 0 {
        return Err(Error::NotSupported(format!(
            "image building with xattrs at {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "hurd"
)))]
fn reject_xattrs(_path: &Path) -> Result<()> {
    Err(Error::NotSupported(
        "directory import without xattr enumeration on this platform".into(),
    ))
}

fn new_source(path: PathBuf, metadata: HostMetadata) -> Result<Source> {
    if !metadata.is_dir() && !metadata.is_file() && !metadata.is_symlink() {
        return Err(Error::NotSupported(format!(
            "image entry type at {}",
            path.display()
        )));
    }
    reject_xattrs(&path)?;
    if !(0..1_000_000_000).contains(&metadata.mtime_nsec()) || metadata.mode() > u32::from(u16::MAX)
    {
        return Err(Error::NotSupported(format!(
            "inode metadata at {}",
            path.display()
        )));
    }
    Ok(Source { path, metadata })
}

fn verify(source: &Source, current: &HostMetadata) -> Result<()> {
    let saved = &source.metadata;
    if current.dev() != saved.dev()
        || current.ino() != saved.ino()
        || current.mode() != saved.mode()
        || current.uid() != saved.uid()
        || current.gid() != saved.gid()
        || current.len() != saved.len()
        || current.nlink() != saved.nlink()
        || current.mtime() != saved.mtime()
        || current.mtime_nsec() != saved.mtime_nsec()
        || current.ctime() != saved.ctime()
        || current.ctime_nsec() != saved.ctime_nsec()
    {
        return Err(io::Error::other(format!(
            "source changed during image build: {}",
            source.path.display()
        ))
        .into());
    }
    Ok(())
}

pub(super) fn write_image(sources: &[Source], output: &mut (impl Write + Seek)) -> Result<()> {
    for source in sources {
        verify(source, &fs::symlink_metadata(&source.path)?)?;
    }
    let mut builder = Builder::new(output)?;
    let mut links = HashMap::new();
    let root = &sources[0].path;
    for source in sources {
        let relative = source.path.strip_prefix(root).map_err(io::Error::other)?;
        let path = UnixPath::new(relative.as_os_str().as_bytes());
        let host = &source.metadata;
        let metadata = Metadata {
            mode: (host.mode() & 0o7777) as u16,
            uid: host.uid(),
            gid: host.gid(),
            modified: (host.mtime(), host.mtime_nsec() as u32),
        };
        if host.is_dir() {
            builder.append_dir(path, metadata)?;
            continue;
        }
        let identity = (host.dev(), host.ino());
        if let Some(target) = links.get(&identity) {
            builder.append_hard_link(path, UnixPath::new(target))?;
            continue;
        }
        if host.is_symlink() {
            let target = fs::read_link(&source.path)?;
            builder.append_symlink(path, metadata, UnixPath::new(target.as_os_str().as_bytes()))?;
        } else {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
                .open(&source.path)?;
            verify(source, &file.metadata()?)?;
            builder.append_file(path, metadata, host.len(), &mut file)?;
            if file.read(&mut [0; 1])? != 0 {
                return Err(io::Error::other("source file grew during image build").into());
            }
            verify(source, &file.metadata()?)?;
        }
        links.insert(identity, path.as_bytes().to_vec());
    }
    for source in sources {
        verify(source, &fs::symlink_metadata(&source.path)?)?;
    }
    builder.finish()?;
    Ok(())
}
