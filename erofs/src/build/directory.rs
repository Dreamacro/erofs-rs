use alloc::{format, string::ToString, vec::Vec};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, Metadata as HostMetadata, OpenOptions},
    io::{self, Read, Seek, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use rustix::fs::OFlags;
use typed_path::UnixPath;

use super::{Metadata, Options, SpecialFile, encode::Content, symlink_target};
use crate::{Error, Result, Xattrs};

pub(super) struct Source {
    path: PathBuf,
    metadata: HostMetadata,
    has_xattrs: bool,
}

/// Creates a new EROFS image from a local directory using [`Builder`](super::Builder).
///
/// Directory import is available on Linux, Android, Apple platforms and Hurd.
/// Other Unix platforms currently return `NotSupported`: missing xattr enumeration
/// must not silently discard attributes. The low-level builder needs no host metadata.
///
/// The destination must not exist and must be outside the source directory.
/// File data is streamed; only paths and metadata are kept in memory. Permissions,
/// UID/GID, modification times, supported xattrs and byte names/targets are preserved.
/// Hard-link counts reflect links inside the image, not outside the source tree. FIFOs and
/// character/block devices preserve their types and device numbers without being
/// opened or read. Sockets are rejected.
///
/// The source tree must remain stable throughout the build. Detectable changes
/// cause an error, but this is not a snapshot or secure traversal of a concurrently
/// mutable tree. The root path is resolved once; symlinks within it are not followed.
/// Unsupported types or xattr namespaces, oversized attributes and attribute read
/// failures are errors, never silently discarded. ACL and security values are opaque.
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
    Options::default().build_from_directory(source, destination)
}

impl Options {
    /// Builds a directory image with these settings.
    ///
    /// Follows the source-stability and output-safety rules of [`from_directory`],
    /// preserving inode modification times. Invalid options are rejected before
    /// accessing source or destination.
    pub fn build_from_directory(
        self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<()> {
        self.validate()?;
        let source = fs::canonicalize(source)?;
        let destination = destination.as_ref();
        let name = destination.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid image output path")
        })?;
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
            write_image(&sources, &mut output, self)
                .and_then(|()| output.sync_all().map_err(Error::from))
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
fn read_xattrs(path: &Path) -> Result<Xattrs> {
    use super::encode::{MAX_XATTR_SIZE, xattr_entry_size};
    use rustix::fs::{lgetxattr, llistxattr};

    let len = llistxattr(path, &mut [0u8; 0]).map_err(io::Error::from)?;
    if len == 0 {
        return Ok(Xattrs::new());
    }
    // Full namespace prefixes make a host name list larger than its disk body,
    // but no representable set of supported names needs twice the body limit.
    if len > 2 * MAX_XATTR_SIZE {
        return Err(Error::Overflow("xattr name list size"));
    }
    let mut names = vec![0; len];
    if llistxattr(path, names.as_mut_slice()).map_err(io::Error::from)? != len
        || names.last() != Some(&0)
    {
        return Err(io::Error::other("source xattr names changed during image build").into());
    }
    let mut attrs = Xattrs::new();
    let mut size = size_of::<crate::types::XattrHeader>();
    for name in names[..len - 1].split(|&byte| byte == 0) {
        let len = lgetxattr(path, name, &mut [0u8; 0]).map_err(io::Error::from)?;
        size += xattr_entry_size(name, len)?;
        if size > MAX_XATTR_SIZE {
            return Err(Error::Overflow("inode xattr size"));
        }
        let mut value = vec![0; len];
        if lgetxattr(path, name, value.as_mut_slice()).map_err(io::Error::from)? != len {
            return Err(io::Error::other("source xattr value changed during image build").into());
        }
        if attrs.insert(name.to_vec(), value).is_some() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "duplicate host xattr name").into(),
            );
        }
    }
    Ok(attrs)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "hurd"
)))]
fn read_xattrs(_path: &Path) -> Result<Xattrs> {
    Err(Error::NotSupported(
        "directory import without xattr enumeration on this platform".into(),
    ))
}

fn special_file(metadata: &HostMetadata) -> Option<SpecialFile> {
    let kind = metadata.file_type();
    if kind.is_fifo() {
        return Some(SpecialFile::Fifo);
    }
    // Decode the host's dev_t; its representation is not the EROFS disk format.
    let major = rustix::fs::major(metadata.rdev() as _);
    let minor = rustix::fs::minor(metadata.rdev() as _);
    if kind.is_char_device() {
        Some(SpecialFile::CharacterDevice { major, minor })
    } else if kind.is_block_device() {
        Some(SpecialFile::BlockDevice { major, minor })
    } else {
        None
    }
}

fn new_source(path: PathBuf, metadata: HostMetadata) -> Result<Source> {
    if let Some(kind) = special_file(&metadata) {
        kind.validate()?;
    } else if !metadata.is_dir() && !metadata.is_file() && !metadata.is_symlink() {
        return Err(Error::NotSupported(format!(
            "image entry type at {}",
            path.display()
        )));
    }
    let has_xattrs = !read_xattrs(&path)?.is_empty();
    if !(0..1_000_000_000).contains(&metadata.mtime_nsec()) || metadata.mode() > u32::from(u16::MAX)
    {
        return Err(Error::NotSupported(format!(
            "inode metadata at {}",
            path.display()
        )));
    }
    Ok(Source {
        path,
        metadata,
        has_xattrs,
    })
}

fn verify(source: &Source, current: &HostMetadata) -> Result<()> {
    let saved = &source.metadata;
    if current.dev() != saved.dev()
        || current.ino() != saved.ino()
        || current.rdev() != saved.rdev()
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

pub(super) fn write_image(
    sources: &[Source],
    output: &mut (impl Write + Seek),
    options: Options,
) -> Result<()> {
    let mut links = HashMap::new();
    for source in sources {
        verify(source, &fs::symlink_metadata(&source.path)?)?;
        if !source.metadata.is_dir() {
            let identity = (source.metadata.dev(), source.metadata.ino());
            let (count, _) = links.entry(identity).or_insert((0u32, None));
            *count = count
                .checked_add(1)
                .ok_or(Error::Overflow("inode link count"))?;
        }
    }
    let mut builder = options.build(output)?;
    let root = &sources[0].path;
    for source in sources {
        let relative = source.path.strip_prefix(root).map_err(io::Error::other)?;
        let path = UnixPath::new(relative.as_os_str().as_bytes());
        let host = &source.metadata;
        let identity = (host.dev(), host.ino());
        if !host.is_dir()
            && let Some((_, Some(target))) = links.get(&identity)
        {
            builder.append_hard_link(path, UnixPath::new(target))?;
            continue;
        }
        let metadata = Metadata {
            mode: (host.mode() & 0o7777) as u16,
            uid: host.uid(),
            gid: host.gid(),
            modified: (host.mtime(), host.mtime_nsec() as u32),
            xattrs: if source.has_xattrs {
                read_xattrs(&source.path)?
            } else {
                Xattrs::new()
            },
        };
        if host.is_dir() {
            builder.append_dir(path, metadata)?;
            continue;
        }
        let nlink = links[&identity].0;
        if host.is_symlink() {
            let target = fs::read_link(&source.path)?;
            let target = symlink_target(UnixPath::new(target.as_os_str().as_bytes()))?;
            builder.append_payload(
                path,
                &metadata,
                Content::Symlink,
                target.len() as u64,
                nlink,
                target,
            )?;
        } else if let Some(kind) = special_file(host) {
            builder.append_payload(path, &metadata, Content::Special(kind), 0, nlink, &[][..])?;
        } else {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
                .open(&source.path)?;
            verify(source, &file.metadata()?)?;
            builder.append_payload(path, &metadata, Content::File, host.len(), nlink, &mut file)?;
            if file.read(&mut [0; 1])? != 0 {
                return Err(io::Error::other("source file grew during image build").into());
            }
            verify(source, &file.metadata()?)?;
        }
        links.get_mut(&identity).unwrap().1 = Some(path.as_bytes().to_vec());
    }
    for source in sources {
        verify(source, &fs::symlink_metadata(&source.path)?)?;
    }
    builder.finish()?;
    Ok(())
}
