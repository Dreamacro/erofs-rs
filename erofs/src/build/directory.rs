use alloc::{format, string::ToString, vec::Vec};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, Metadata as HostMetadata, OpenOptions},
    io::{self, Seek, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    thread,
};
#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
use std::{
    io::Read,
    sync::{Arc, Condvar, Mutex, mpsc},
};

use rustix::fs::OFlags;
use typed_path::UnixPath;

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
use super::compress::{Encoder, FittedFile, SEGMENT_SIZE};
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
/// File data is streamed or prefetched in bounded segments. Permissions,
/// UID/GID, modification times, supported xattrs and byte names/targets are preserved.
/// Hard-link counts reflect links inside the image, not outside the source tree. FIFOs and
/// character/block devices preserve their types and device numbers without being
/// opened or read. Sockets are rejected.
///
/// The source tree must remain stable throughout the build. Detectable changes
/// cause an error, but this is not a snapshot or secure traversal of a concurrently
/// mutable tree; regular files are checked while read, not afterwards. The root path is
/// resolved once; symlinks within it are not followed.
/// Unsupported types, oversized attributes and attribute read failures are errors.
/// Xattrs outside EROFS namespaces are skipped unless
/// [`Options::skip_unsupported_xattrs`] is disabled. ACL and security values are opaque.
/// Failed builds remove their incomplete output, reporting cleanup failures as well.
/// Like `mkfs.erofs`, the output is not synced; call `File::sync_all` for durability.
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
        let sources = scan(&source, self)?;
        let result = {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            write_image(&sources, &mut output, self)
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

pub(super) fn scan(root: &Path, options: Options) -> Result<Vec<Source>> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Err(Error::NotADirectory(root.display().to_string()));
    }
    let mut directories = HashSet::from([(metadata.dev(), metadata.ino())]);
    let mut sources = vec![new_source(root.to_path_buf(), metadata, options)?];
    // Iterate the growing list instead of recursing on host directory depth.
    let mut index = 0;
    while index < sources.len() {
        if sources[index].metadata.is_dir() {
            let mut children =
                fs::read_dir(&sources[index].path)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_cached_key(|child| child.file_name());
            for child in children {
                // Relative to the open directory; does not follow symlinks.
                let metadata = child.metadata()?;
                if metadata.is_dir() && !directories.insert((metadata.dev(), metadata.ino())) {
                    return Err(Error::NotSupported(format!(
                        "directory cycle or alias at {}",
                        child.path().display()
                    )));
                }
                sources.push(new_source(child.path(), metadata, options)?);
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
fn read_xattrs(path: &Path, skip_unsupported: bool) -> Result<Xattrs> {
    use super::encode::{MAX_XATTR_SIZE, xattr_entry_size};
    use rustix::fs::{lgetxattr, lgetxattr_query, llistxattr, llistxattr_query};

    // rustix 1.1.6+ no longer treats an empty buffer as a size query.
    let len = llistxattr_query(path).map_err(io::Error::from)?;
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
        let Some(suffix) = importable_suffix(name, skip_unsupported)? else {
            continue;
        };
        let len = lgetxattr_query(path, name).map_err(io::Error::from)?;
        size += xattr_entry_size(suffix.len(), len)?;
        if size > MAX_XATTR_SIZE {
            return Err(Error::Overflow("inode xattr size"));
        }
        let mut value = vec![0; len];
        if lgetxattr(path, name, value.as_mut_slice()).map_err(io::Error::from)? != len {
            return Err(io::Error::other("source xattr value changed during image build").into());
        }
        attrs.insert(name.to_vec(), value);
    }
    Ok(attrs)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "hurd"
))]
pub(super) fn importable_suffix(name: &[u8], skip_unsupported: bool) -> Result<Option<&[u8]>> {
    match super::encode::xattr_name(name) {
        Ok((_, suffix)) => Ok(Some(suffix)),
        Err(Error::NotSupported(_)) if skip_unsupported => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "hurd"
)))]
fn read_xattrs(_path: &Path, _skip_unsupported: bool) -> Result<Xattrs> {
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

fn new_source(path: PathBuf, metadata: HostMetadata, options: Options) -> Result<Source> {
    if let Some(kind) = special_file(&metadata) {
        kind.validate()?;
    } else if !metadata.is_dir() && !metadata.is_file() && !metadata.is_symlink() {
        return Err(Error::NotSupported(format!(
            "image entry type at {}",
            path.display()
        )));
    }
    let has_xattrs = !read_xattrs(&path, options.skip_unsupported_xattrs)?.is_empty();
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
    thread::scope(|scope| append_sources(scope, sources, output, options))
}

fn append_sources<'scope, 'env>(
    scope: &'scope thread::Scope<'scope, 'env>,
    sources: &'env [Source],
    output: &mut (impl Write + Seek),
    options: Options,
) -> Result<()> {
    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    let mut prefetch = Prefetch::start(scope, sources, options);
    #[cfg(not(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    )))]
    let _ = scope;
    let mut links = HashMap::new();
    for source in sources {
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
                read_xattrs(&source.path, options.skip_unsupported_xattrs)?
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
            #[cfg(any(
                feature = "lz4",
                feature = "lzma",
                feature = "deflate",
                feature = "zstd"
            ))]
            if let Some(prepared) = prefetch.take(source) {
                let Prepared { input, fitted } = prepared?;
                if options.buffers_file(host.len()) {
                    builder.append_fitted(path, &metadata, nlink, &input, fitted)?;
                } else {
                    let mut file = builder.begin_segments(path, &metadata, host.len(), nlink)?;
                    builder.append_segment(&mut file, fitted)?;
                    while let Some(prepared) = prefetch.take(source) {
                        builder.append_segment(&mut file, prepared?.fitted)?;
                    }
                    builder.finish_segments(file)?;
                }
                links.get_mut(&identity).unwrap().1 = Some(path.as_bytes().to_vec());
                continue;
            }
            let mut file = open_file(&source.path)?;
            verify(source, &file.metadata()?)?;
            builder.append_payload(path, &metadata, Content::File, host.len(), nlink, &mut file)?;
            // Size and ctime also reveal growth or edits during the read.
            verify(source, &file.metadata()?)?;
        }
        links.get_mut(&identity).unwrap().1 = Some(path.as_bytes().to_vec());
    }
    // Files were verified around their reads; directory times reveal renames and replacements.
    for source in sources.iter().filter(|source| !source.metadata.is_file()) {
        verify(source, &fs::symlink_metadata(&source.path)?)?;
    }
    builder.finish()?;
    Ok(())
}

fn open_file(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
struct Prepared {
    input: Vec<u8>,
    fitted: FittedFile,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[derive(Clone, Copy)]
struct Segment {
    job: usize,
    source: usize,
    offset: u64,
    len: usize,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
impl Segment {
    fn charge(self) -> usize {
        2 * self.len.next_multiple_of(super::encode::BLOCK_SIZE)
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[derive(Clone, Copy, Default)]
struct Cursor {
    job: usize,
    source: usize,
    offset: u64,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
impl Cursor {
    fn next(&mut self, sources: &[Source], jobs: &[usize]) -> Option<Segment> {
        let &source = jobs.get(self.source)?;
        let size = sources[source].metadata.len();
        let len = (size - self.offset).min(SEGMENT_SIZE as u64) as usize;
        let segment = Segment {
            job: self.job,
            source,
            offset: self.offset,
            len,
        };
        self.job += 1;
        self.offset += len as u64;
        if self.offset == size {
            self.source += 1;
            self.offset = 0;
        }
        Some(segment)
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
fn prepare(
    source: &Source,
    segment: Segment,
    options: &Options,
    encoder: &mut Encoder,
) -> Result<Prepared> {
    let mut file = open_file(&source.path)?;
    verify(source, &file.metadata()?)?;
    if segment.offset != 0 {
        file.seek(io::SeekFrom::Start(segment.offset))?;
    }
    let mut input = vec![0; segment.len];
    file.read_exact(&mut input)?;
    verify(source, &file.metadata()?)?;
    let fitted = FittedFile::fit(options, encoder, &input)?;
    // Only whole short files need their input for flat fallback at commit time.
    if !options.buffers_file(source.metadata.len()) {
        input = Vec::new();
    }
    Ok(Prepared { input, fitted })
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[derive(Default)]
struct Window {
    state: Mutex<WindowState>,
    changed: Condvar,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[derive(Default)]
struct WindowState {
    cursor: Cursor,
    consumed: usize,
    bytes: usize,
    closed: bool,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
impl Window {
    fn claim(
        &self,
        sources: &[Source],
        jobs: &[usize],
        capacity: usize,
        byte_capacity: usize,
    ) -> Option<Segment> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            let mut cursor = state.cursor;
            let segment = cursor.next(sources, jobs)?;
            if segment.job - state.consumed < capacity
                && state.bytes + segment.charge() <= byte_capacity
            {
                state.cursor = cursor;
                state.bytes += segment.charge();
                return Some(segment);
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    fn update(&self, change: impl FnOnce(&mut WindowState)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
struct Prefetch<'env> {
    sources: &'env [Source],
    jobs: Arc<[usize]>,
    cursor: Cursor,
    mode: Mode,
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
enum Mode {
    Inline {
        options: Options,
        encoder: Encoder,
    },
    Workers {
        window: Arc<Window>,
        results: mpsc::Receiver<(usize, Result<Prepared>)>,
        ready: HashMap<usize, Result<Prepared>>,
    },
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
impl<'env> Prefetch<'env> {
    fn start<'scope>(
        scope: &'scope thread::Scope<'scope, 'env>,
        sources: &'env [Source],
        options: Options,
    ) -> Self {
        let mut seen = HashSet::new();
        let jobs: Arc<[usize]> = sources
            .iter()
            .enumerate()
            .filter(|(_, source)| {
                let host = &source.metadata;
                host.is_file()
                    && seen.insert((host.dev(), host.ino()))
                    && options.prefits_file(host.len())
            })
            .map(|(index, _)| index)
            .collect();
        let mode = if options.workers == 1 {
            Mode::Inline {
                options,
                encoder: Encoder::new(&options),
            }
        } else {
            Self::spawn(scope, sources, &jobs, options)
        };
        Self {
            sources,
            jobs,
            cursor: Cursor::default(),
            mode,
        }
    }

    fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, 'env>,
        sources: &'env [Source],
        jobs: &Arc<[usize]>,
        options: Options,
    ) -> Mode {
        let window = Arc::new(Window::default());
        let (sender, results) = mpsc::channel();
        let capacity = options.workers.saturating_mul(8);
        let byte_capacity = options.workers.saturating_mul(2 * SEGMENT_SIZE);
        let segments = jobs
            .iter()
            .map(|&source| sources[source].metadata.len().div_ceil(SEGMENT_SIZE as u64))
            .sum::<u64>();
        for _ in 0..options
            .workers
            .min(usize::try_from(segments).unwrap_or(usize::MAX))
        {
            let (jobs, window, sender) = (Arc::clone(jobs), Arc::clone(&window), sender.clone());
            scope.spawn(move || {
                let mut encoder = Encoder::new(&options);
                while let Some(segment) = window.claim(sources, &jobs, capacity, byte_capacity) {
                    let result = prepare(&sources[segment.source], segment, &options, &mut encoder);
                    if sender.send((segment.job, result)).is_err() {
                        break;
                    }
                }
            });
        }
        Mode::Workers {
            window,
            results,
            ready: HashMap::new(),
        }
    }

    fn take(&mut self, source: &Source) -> Option<Result<Prepared>> {
        if !self
            .jobs
            .get(self.cursor.source)
            .is_some_and(|&job| core::ptr::eq(&self.sources[job], source))
        {
            return None;
        }
        let segment = self.cursor.next(self.sources, &self.jobs)?;
        let (window, results, ready) = match &mut self.mode {
            Mode::Inline { options, encoder } => {
                return Some(prepare(source, segment, options, encoder));
            }
            Mode::Workers {
                window,
                results,
                ready,
            } => (window, results, ready),
        };
        let result = loop {
            if let Some(result) = ready.remove(&segment.job) {
                break result;
            }
            match results.recv() {
                Ok((done, result)) => {
                    ready.insert(done, result);
                }
                Err(_) => {
                    return Some(Err(io::Error::other("compression worker stopped").into()));
                }
            }
        };
        window.update(|state| {
            state.consumed = segment.job + 1;
            state.bytes -= segment.charge();
        });
        Some(result)
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
impl Drop for Prefetch<'_> {
    fn drop(&mut self) {
        if let Mode::Workers { window, .. } = &self.mode {
            window.update(|state| state.closed = true);
        }
    }
}
