use alloc::{boxed::Box, format, string::ToString, vec::Vec};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, Metadata as HostMetadata, OpenOptions},
    io::{self, Seek, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};
#[cfg(codecs)]
use std::{
    io::Read,
    sync::{Arc, Condvar, Mutex, mpsc},
};

use rustix::fs::OFlags;
use typed_path::UnixPath;

#[cfg(codecs)]
use super::compress::{Encoder, FittedFile, INPUT_SIZE, SEGMENT_SIZE};
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
    let mut entries = vec![(root.to_path_buf(), Some(Box::new(metadata)))];
    // Iterate the growing list instead of recursing on host directory depth.
    let mut index = 0;
    while index < entries.len() {
        if entries[index]
            .1
            .as_deref()
            .is_some_and(HostMetadata::is_dir)
        {
            let mut children = fs::read_dir(&entries[index].0)?.collect::<io::Result<Vec<_>>>()?;
            children.sort_by_cached_key(|child| child.file_name());
            for child in children {
                // Other entries are stat'ed by `describe`.
                let metadata = if child.file_type()?.is_dir() {
                    // Relative to the open directory; does not follow symlinks.
                    let metadata = child.metadata()?;
                    if metadata.is_dir() && !directories.insert((metadata.dev(), metadata.ino())) {
                        return Err(Error::NotSupported(format!(
                            "directory cycle or alias at {}",
                            child.path().display()
                        )));
                    }
                    Some(Box::new(metadata))
                } else {
                    None
                };
                entries.push((child.path(), metadata));
            }
        }
        index += 1;
    }
    describe(entries, options)
}

fn describe(
    entries: Vec<(PathBuf, Option<Box<HostMetadata>>)>,
    options: Options,
) -> Result<Vec<Source>> {
    const CHUNK: usize = 64;
    let describe = |path: &Path, metadata: Option<HostMetadata>| {
        let metadata = match metadata {
            Some(metadata) => metadata,
            None => {
                let metadata = fs::symlink_metadata(path)?;
                if metadata.is_dir() {
                    return Err(io::Error::other(format!(
                        "source changed during image build: {}",
                        path.display()
                    ))
                    .into());
                }
                metadata
            }
        };
        let has_xattrs = check_source(path, &metadata, options)?;
        Ok((metadata, has_xattrs))
    };
    let workers = options.workers.min(entries.len().div_ceil(CHUNK));
    if workers <= 1 {
        return entries
            .into_iter()
            .map(|(path, metadata)| {
                let (metadata, has_xattrs) = describe(&path, metadata.map(|metadata| *metadata))?;
                Ok(Source {
                    path,
                    metadata,
                    has_xattrs,
                })
            })
            .collect();
    }
    let results: Vec<OnceLock<Result<_>>> = entries.iter().map(|_| OnceLock::new()).collect();
    let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
    thread::scope(|scope| {
        for _ in 0..workers {
            let spawned = thread::Builder::new().spawn_scoped(scope, || {
                while !failed.load(Ordering::Relaxed) {
                    let start = next.fetch_add(CHUNK, Ordering::Relaxed);
                    if start >= entries.len() {
                        break;
                    }
                    for index in start..(start + CHUNK).min(entries.len()) {
                        let (path, metadata) = &entries[index];
                        let result = describe(path, metadata.as_deref().cloned());
                        if result.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        let _ = results[index].set(result);
                    }
                }
            });
            if let Err(error) = spawned {
                failed.store(true, Ordering::Relaxed);
                return Err(error);
            }
        }
        Ok(())
    })?;
    // Chunks are claimed in order, so no gap precedes the first error.
    let described = results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .expect("described before the first error")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(entries
        .into_iter()
        .zip(described)
        .map(|((path, _), (metadata, has_xattrs))| Source {
            path,
            metadata,
            has_xattrs,
        })
        .collect())
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

// Returns whether the entry has supported xattrs.
fn check_source(path: &Path, metadata: &HostMetadata, options: Options) -> Result<bool> {
    if let Some(kind) = special_file(metadata) {
        kind.validate()?;
    } else if !metadata.is_dir() && !metadata.is_file() && !metadata.is_symlink() {
        return Err(Error::NotSupported(format!(
            "image entry type at {}",
            path.display()
        )));
    }
    let has_xattrs = !read_xattrs(path, options.skip_unsupported_xattrs)?.is_empty();
    if !(0..1_000_000_000).contains(&metadata.mtime_nsec()) || metadata.mode() > u32::from(u16::MAX)
    {
        return Err(Error::NotSupported(format!(
            "inode metadata at {}",
            path.display()
        )));
    }
    Ok(has_xattrs)
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
    #[cfg(codecs)]
    let mut prefetch = Prefetch::start(scope, sources, options);
    #[cfg(not(codecs))]
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
            #[cfg(codecs)]
            if let Some(prepared) = prefetch.take(source) {
                match prepared? {
                    Prepared::Flat(input) => builder.append_payload(
                        path,
                        &metadata,
                        Content::File,
                        host.len(),
                        nlink,
                        &input[..],
                    )?,
                    Prepared::Buffered { input, fitted } => {
                        builder.append_fitted(path, &metadata, nlink, &input, fitted)?;
                    }
                    Prepared::Segment(fitted) => {
                        let mut file =
                            builder.begin_segments(path, &metadata, host.len(), nlink)?;
                        builder.append_segment(&mut file, fitted)?;
                        while let Some(prepared) = prefetch.take(source) {
                            let Prepared::Segment(fitted) = prepared? else {
                                return Err(io::Error::other(
                                    "whole-file result in a segmented file",
                                )
                                .into());
                            };
                            builder.append_segment(&mut file, fitted)?;
                        }
                        builder.finish_segments(file)?;
                    }
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

#[cfg(codecs)]
enum Prepared {
    Flat(Vec<u8>),
    Buffered { input: Vec<u8>, fitted: FittedFile },
    Segment(FittedFile),
}

#[cfg(codecs)]
impl Prepared {
    fn held(result: &Result<Self>) -> usize {
        result.as_ref().map_or(0, |prepared| match prepared {
            Self::Flat(input) => input.len(),
            Self::Buffered { input, fitted } => input.len() + fitted.data.len(),
            Self::Segment(fitted) => fitted.data.len(),
        })
    }
}

#[cfg(codecs)]
#[derive(Clone, Copy)]
struct Segment {
    job: usize,
    source: usize,
    offset: u64,
    len: usize,
}

#[cfg(codecs)]
impl Segment {
    fn charge(self) -> usize {
        self.len.next_multiple_of(super::encode::BLOCK_SIZE) + self.len.min(INPUT_SIZE)
    }
}

#[cfg(codecs)]
#[derive(Clone, Copy, Default)]
struct Cursor {
    job: usize,
    source: usize,
    offset: u64,
}

#[cfg(codecs)]
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

#[cfg(codecs)]
fn prepare(
    source: &Source,
    segment: Segment,
    options: &Options,
    encoder: &mut Encoder,
    buffer: &mut Vec<u8>,
) -> Result<Prepared> {
    let mut file = open_file(&source.path)?;
    verify(source, &file.metadata()?)?;
    // Whole short files retain their input for flat storage or compression fallback.
    let prepared = if source.metadata.len() <= INPUT_SIZE as u64 {
        let mut input = vec![0; segment.len];
        file.read_exact(&mut input)?;
        if options.buffers_file(source.metadata.len()) {
            let fitted = FittedFile::fit(options, encoder, &input)?;
            Prepared::Buffered { input, fitted }
        } else {
            Prepared::Flat(input)
        }
    } else {
        if segment.offset != 0 {
            file.seek(io::SeekFrom::Start(segment.offset))?;
        }
        let fitted = FittedFile::fit_from(options, encoder, segment.len, &mut file, buffer)?;
        Prepared::Segment(fitted)
    };
    verify(source, &file.metadata()?)?;
    Ok(prepared)
}

#[cfg(codecs)]
struct Window {
    state: Mutex<WindowState>,
    changed: Condvar,
    capacity: usize,
    byte_capacity: usize,
}

#[cfg(codecs)]
#[derive(Default)]
struct WindowState {
    cursor: Cursor,
    consumed: usize,
    bytes: usize,
    closed: bool,
}

#[cfg(codecs)]
impl Window {
    fn new(capacity: usize, byte_capacity: usize) -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            capacity,
            byte_capacity,
        }
    }

    fn can_claim(&self, state: &WindowState, segment: Segment) -> bool {
        segment.job - state.consumed < self.capacity
            && state.bytes + segment.charge() <= self.byte_capacity
    }

    fn claim(&self, sources: &[Source], jobs: &[usize]) -> Option<Segment> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            let mut cursor = state.cursor;
            let segment = cursor.next(sources, jobs)?;
            if self.can_claim(&state, segment) {
                state.cursor = cursor;
                state.bytes += segment.charge();
                let next = cursor.next(sources, jobs);
                let wake_next = next.is_some_and(|next| self.can_claim(&state, next));
                drop(state);
                if next.is_none() {
                    self.changed.notify_all();
                } else if wake_next {
                    // A byte-budget release can admit more than one job.
                    self.changed.notify_one();
                }
                return Some(segment);
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    fn prepared(&self, segment: Segment, held: usize) {
        let mut state = self.state.lock().unwrap();
        state.bytes = state.bytes + held - segment.charge();
        let wake = held < segment.charge()
            && state.cursor.job - state.consumed < self.capacity
            && !state.closed;
        drop(state);
        if wake {
            self.changed.notify_one();
        }
    }

    fn consume(&self, job: usize, held: usize) {
        let mut state = self.state.lock().unwrap();
        state.consumed = job + 1;
        state.bytes -= held;
        drop(state);
        self.changed.notify_one();
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

#[cfg(codecs)]
struct Prefetch<'env> {
    sources: &'env [Source],
    jobs: Arc<[usize]>,
    cursor: Cursor,
    mode: Mode,
}

#[cfg(codecs)]
enum Mode {
    Inline {
        options: Options,
        encoder: Encoder,
        buffer: Vec<u8>,
    },
    Workers {
        window: Arc<Window>,
        results: mpsc::Receiver<(usize, Result<Prepared>)>,
        ready: HashMap<usize, Result<Prepared>>,
    },
}

#[cfg(codecs)]
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
                    && options.prefetches_file(host.len())
            })
            .map(|(index, _)| index)
            .collect();
        let mode = if options.workers == 1 {
            Mode::Inline {
                options,
                encoder: Encoder::new(&options),
                buffer: Vec::new(),
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
        let window = Arc::new(Window::new(
            options.workers.saturating_mul(32),
            options.workers.saturating_mul(2 * SEGMENT_SIZE),
        ));
        let (sender, results) = mpsc::channel();
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
                let mut buffer = Vec::new();
                while let Some(segment) = window.claim(sources, &jobs) {
                    let source = &sources[segment.source];
                    let result = prepare(source, segment, &options, &mut encoder, &mut buffer);
                    let held = Prepared::held(&result);
                    window.prepared(segment, held);
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
            Mode::Inline {
                options,
                encoder,
                buffer,
            } => return Some(prepare(source, segment, options, encoder, buffer)),
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
                    return Some(Err(
                        io::Error::other("file preparation worker stopped").into()
                    ));
                }
            }
        };
        let held = Prepared::held(&result);
        window.consume(segment.job, held);
        Some(result)
    }
}

#[cfg(codecs)]
impl Drop for Prefetch<'_> {
    fn drop(&mut self) {
        if let Mode::Workers { window, .. } = &self.mode {
            window.close();
        }
    }
}

#[cfg(all(
    test,
    codecs,
    any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "hurd"
    )
))]
mod tests {
    use super::super::{
        Compression,
        encode::BLOCK_SIZE,
        tests::{COMPRESSIONS, directory::Temp},
    };
    use super::*;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(10);

    struct UnblockOnDrop<'a>(&'a Window);

    impl Drop for UnblockOnDrop<'_> {
        fn drop(&mut self) {
            // Clean up even when the notification under test is broken.
            self.0
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed = true;
            self.0.changed.notify_all();
        }
    }

    fn window_sources(sizes: &[usize]) -> (Temp, Vec<Source>, Vec<usize>) {
        let temp = Temp::new();
        for (index, &size) in sizes.iter().enumerate() {
            fs::File::create(temp.0.join(format!("file-{index}")))
                .unwrap()
                .set_len(size as u64)
                .unwrap();
        }
        let sources = scan(&temp.0, Options::default()).unwrap();
        let jobs = sources
            .iter()
            .enumerate()
            .filter(|(_, source)| source.metadata.is_file())
            .map(|(index, _)| index)
            .collect();
        (temp, sources, jobs)
    }

    fn parked_claimants<'scope, 'env>(
        scope: &'scope thread::Scope<'scope, 'env>,
        window: &'env Window,
        sources: &'env [Source],
        jobs: &'env [usize],
        count: usize,
    ) -> mpsc::Receiver<Option<Segment>> {
        let (waiting, registered) = mpsc::channel();
        let (finished, results) = mpsc::channel();
        for _ in 0..count {
            let (waiting, finished) = (waiting.clone(), finished.clone());
            scope.spawn(move || {
                let state = window.state.lock().unwrap();
                let _ = waiting.send(());
                let state = window
                    .changed
                    .wait_while(state, |state| {
                        let mut cursor = state.cursor;
                        !state.closed
                            && cursor
                                .next(sources, jobs)
                                .is_some_and(|next| !window.can_claim(state, next))
                    })
                    .unwrap();
                drop(state);
                let _ = finished.send(window.claim(sources, jobs));
            });
        }
        for _ in 0..count {
            registered.recv_timeout(WAIT).unwrap();
        }
        // Each readiness send holds this mutex until the atomic wait releases it.
        drop(window.state.lock().unwrap());
        results
    }

    #[test]
    fn window_job_slots_and_exhaustion_wake_waiters() {
        let (_temp, sources, jobs) = window_sources(&[1, 1, 1]);
        let window = Window::new(1, 8 * BLOCK_SIZE);
        let first = window.claim(&sources, &jobs).unwrap();
        thread::scope(|scope| {
            let _cleanup = UnblockOnDrop(&window);
            let results = parked_claimants(scope, &window, &sources, &jobs, 4);
            window.prepared(first, 1);
            assert!(matches!(results.try_recv(), Err(mpsc::TryRecvError::Empty)));
            window.consume(first.job, 1);
            let second = results.recv_timeout(WAIT).unwrap().unwrap();
            assert_eq!(second.job, 1);
            assert!(matches!(results.try_recv(), Err(mpsc::TryRecvError::Empty)));
            window.prepared(second, 1);
            window.consume(second.job, 1);
            let claimed: Vec<_> = (0..3)
                .filter_map(|_| results.recv_timeout(WAIT).unwrap())
                .collect();
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].job, 2);
            // Exhaustion must wake the other two waiters before close or further progress.
            window.prepared(claimed[0], 1);
            window.consume(claimed[0].job, 1);
            let WindowState {
                bytes, consumed, ..
            } = *window.state.lock().unwrap();
            assert_eq!((bytes, consumed), (0, 3));
        });
    }

    #[test]
    fn window_byte_releases_pass_the_wakeup_to_all_admissible_jobs() {
        let (_temp, sources, jobs) = window_sources(&[INPUT_SIZE, 1, 1, 1]);
        for held in [0, INPUT_SIZE + BLOCK_SIZE, 2 * INPUT_SIZE] {
            let window = Window::new(4, 2 * INPUT_SIZE);
            let first = window.claim(&sources, &jobs).unwrap();
            thread::scope(|scope| {
                let _cleanup = UnblockOnDrop(&window);
                let results = parked_claimants(scope, &window, &sources, &jobs, 3);
                window.prepared(first, held);
                let consumed = held == first.charge();
                if consumed {
                    window.consume(first.job, held);
                }
                // No claimant finishes or consumes its job to trigger another notification.
                let mut claimed: Vec<_> = (0..3)
                    .map(|_| results.recv_timeout(WAIT).unwrap().unwrap())
                    .collect();
                claimed.sort_unstable_by_key(|segment| segment.job);
                assert_eq!(
                    claimed
                        .iter()
                        .map(|segment| segment.job)
                        .collect::<Vec<_>>(),
                    [1, 2, 3]
                );
                let expected_bytes = claimed
                    .iter()
                    .map(|segment| segment.charge())
                    .sum::<usize>()
                    + if consumed { 0 } else { held };
                let WindowState {
                    bytes,
                    consumed: count,
                    ..
                } = *window.state.lock().unwrap();
                assert_eq!((bytes, count), (expected_bytes, usize::from(consumed)));
            });
        }
    }

    #[test]
    fn window_close_wakes_all_waiters() {
        let (_temp, sources, jobs) = window_sources(&[1, 1]);
        let window = Window::new(1, 8 * BLOCK_SIZE);
        window.claim(&sources, &jobs).unwrap();
        thread::scope(|scope| {
            let _cleanup = UnblockOnDrop(&window);
            let results = parked_claimants(scope, &window, &sources, &jobs, 4);
            window.close();
            for _ in 0..4 {
                assert!(results.recv_timeout(WAIT).unwrap().is_none());
            }
            assert!(window.claim(&sources, &jobs).is_none());
        });
    }

    #[test]
    fn prefetch_selects_and_prepares_file_kinds() {
        let temp = Temp::new();
        for (index, size) in [
            0,
            1,
            BLOCK_SIZE - 1,
            BLOCK_SIZE,
            BLOCK_SIZE + 1,
            INPUT_SIZE,
            INPUT_SIZE + 1,
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(temp.0.join(format!("file-{index}")), vec![42; size]).unwrap();
        }
        fs::hard_link(temp.0.join("file-3"), temp.0.join("z-hardlink")).unwrap();
        std::os::unix::fs::symlink("file-3", temp.0.join("z-symlink")).unwrap();
        let sources = scan(&temp.0, Options::default()).unwrap();
        let compression = COMPRESSIONS
            .into_iter()
            .find(|codec| *codec != Compression::None && codec.enabled())
            .unwrap();
        let options = Options::default().compression(compression).workers(2);
        for (options, prepares) in [
            (options.workers(1), true),
            (options, true),
            (options.compression(Compression::None), false),
            (options.inline_compressed_tails(true), false),
            (options.fragments(true), false),
        ] {
            thread::scope(|scope| {
                let mut prefetch = Prefetch::start(scope, &sources, options);
                assert_eq!(
                    matches!(&prefetch.mode, Mode::Inline { .. }),
                    options.workers == 1
                );
                for (index, source) in sources.iter().enumerate() {
                    let size = source.metadata.len() as usize;
                    let original_file = source
                        .path
                        .file_name()
                        .unwrap()
                        .as_bytes()
                        .starts_with(b"file-");
                    let eligible = original_file
                        && prepares
                        && size != 0
                        && (options.workers > 1 || size > BLOCK_SIZE);
                    assert_eq!(prefetch.jobs.contains(&index), eligible);
                    let result = prefetch.take(source);
                    if !eligible {
                        assert!(result.is_none());
                        continue;
                    }
                    let result = result.expect("eligible file must be prepared");
                    let held = Prepared::held(&result);
                    match result.unwrap() {
                        Prepared::Flat(input) => {
                            assert!((1..=BLOCK_SIZE).contains(&size));
                            assert!(options.workers > 1);
                            assert_eq!(input, vec![42; size]);
                            assert_eq!(held, size);
                        }
                        Prepared::Buffered { input, fitted } => {
                            assert!(size > BLOCK_SIZE && size <= INPUT_SIZE);
                            assert_eq!(input, vec![42; size]);
                            assert!(!fitted.data.is_empty());
                            assert_eq!(held, size + fitted.data.len());
                        }
                        Prepared::Segment(fitted) => {
                            assert!(size > INPUT_SIZE);
                            assert!(!fitted.data.is_empty());
                            assert_eq!(held, fitted.data.len());
                        }
                    }
                    assert!(prefetch.take(source).is_none());
                }
                if let Mode::Workers { window, .. } = &prefetch.mode {
                    let WindowState {
                        consumed, bytes, ..
                    } = *window.state.lock().unwrap();
                    assert_eq!(consumed, prefetch.jobs.len());
                    assert_eq!(bytes, 0);
                }
            });
        }
        assert_eq!(
            Prepared::held(&Err(io::Error::other("test error").into())),
            0
        );
    }
}
