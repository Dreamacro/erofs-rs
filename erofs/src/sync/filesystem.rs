use alloc::{borrow::Cow, format, string::ToString, sync::Arc, vec::Vec};
use typed_path::{UnixPath, UnixPathBuf};

use super::file::File;
use super::walkdir::WalkDir;
use crate::backend::Image;
use crate::dirent;
use crate::filesystem::{BlockPlan, EroFSCore};
use crate::types::*;
use crate::xattr::XattrRead;
use crate::{DeviceInfo, Error, Result, Xattrs};

/// The main entry point for reading EROFS filesystem images.
///
/// `EroFS` provides methods to traverse directories, open files, and access
/// filesystem metadata from EROFS images. It supports both standard (mmap-based)
/// and no_std (slice-based) backends.
///
/// Paths are resolved component by component from the image root, including `.`
/// and `..`. Symbolic links are not followed; a trailing slash requires a directory.
///
/// # Examples
///
/// ## Standard usage with mmap
///
/// ```no_run
/// # #[cfg(feature = "std")]
/// # {
/// use std::io::Read;
/// use erofs_rs::{EroFS, backend::MmapImage};
///
/// // SAFETY: assume the file remains immutable until the filesystem is dropped.
/// let image = unsafe { MmapImage::new_from_path("image.erofs").unwrap() };
/// let fs = EroFS::new(image).unwrap();
///
/// let mut file = fs.open("/etc/passwd").unwrap();
/// let mut content = String::new();
/// file.read_to_string(&mut content).unwrap();
/// # }
/// ```
///
/// ## no_std usage with byte slice
///
/// ```no_run
/// # extern crate alloc;
/// use erofs_rs::{EroFS, backend::SliceImage};
///
/// let image_data: &'static [u8] = &[/* EROFS image data */];
/// let fs = EroFS::new(SliceImage::new(image_data)).unwrap();
///
/// // Traverse directories
/// for entry in fs.read_dir("/etc").unwrap() {
///     let entry = entry.unwrap();
///     // Process directory entry...
/// }
/// ```
#[derive(Debug)]
pub struct EroFS<I: Image> {
    image: Arc<I>,
    devices: Arc<[I]>,
    core: EroFSCore,
}

impl<I: Image> Clone for EroFS<I> {
    fn clone(&self) -> Self {
        Self {
            image: Arc::clone(&self.image),
            devices: Arc::clone(&self.devices),
            core: self.core.clone(),
        }
    }
}

impl<I: Image> EroFS<I> {
    /// Creates a new `EroFS` instance from a backend image source.
    ///
    /// The backend can be either a memory-mapped file ([`MmapImage`](crate::backend::MmapImage))
    /// in std environments, or a byte slice ([`SliceImage`](crate::backend::SliceImage)) in
    /// no_std environments.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The superblock cannot be read
    /// - The magic number doesn't match EROFS format (0xE0F5E1E2)
    /// - The block size is invalid (must be 2^n where 9 ≤ n ≤ 24)
    /// - The image requires an unsupported incompatible feature or additional images
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[cfg(feature = "std")]
    /// # {
    /// use erofs_rs::{EroFS, backend::MmapImage};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// // SAFETY: assume the file remains immutable until the filesystem is dropped.
    /// let image = unsafe { MmapImage::new_from_path("image.erofs")? };
    /// let fs = EroFS::new(image)?;
    /// # Ok(())
    /// # }
    /// # }
    /// ```
    pub fn new(image: I) -> Result<Self> {
        Self::new_with_devices(image, Vec::new())
    }

    /// Opens an image with additional backing images in on-disk device-table order.
    /// The number of supplied images must match the active device table.
    /// All mappings, including additional devices, must remain immutable.
    pub fn new_with_devices(image: I, devices: Vec<I>) -> Result<Self> {
        let sb_data = image
            .get(SUPER_BLOCK_OFFSET..SUPER_BLOCK_OFFSET + SuperBlock::size() as u64)
            .ok_or_else(|| Error::InvalidSuperblock("failed to read super block".to_string()))?;
        let mut core = EroFSCore::new(sb_data)?;
        let range = core.device_table_range(devices.len())?;
        if !range.is_empty() {
            let data = image
                .get(range.clone())
                .filter(|data| data.len() as u64 == range.end - range.start)
                .ok_or_else(|| Error::OutOfBounds("truncated device table".into()))?;
            core.set_device_table(data)?;
        }
        Ok(Self {
            image: image.into(),
            devices: devices.into(),
            core,
        })
    }

    /// Additional device descriptors; the first entry has device ID 1.
    pub fn devices(&self) -> &[DeviceInfo] {
        self.core.devices()
    }

    fn image_for_device(&self, id: u16) -> Result<&I> {
        if id == 0 {
            return Ok(&self.image);
        }
        self.devices
            .get(usize::from(id) - 1)
            .ok_or_else(|| Error::OutOfBounds(format!("missing image for device {id}")))
    }

    /// Recursively walks a directory tree starting from the given path.
    ///
    /// Returns an iterator that yields all entries (files and directories)
    /// under the specified root path.
    pub fn walk_dir<P: AsRef<UnixPath>>(&self, root: P) -> Result<WalkDir<'_, I>> {
        WalkDir::new(self, root)
    }

    /// Lists the immediate contents of a directory.
    ///
    /// This is equivalent to `walk_dir` with `max_depth(1)`.
    pub fn read_dir<P: AsRef<UnixPath>>(&self, path: P) -> Result<WalkDir<'_, I>> {
        Ok(WalkDir::new(self, path)?.max_depth(1))
    }

    /// Opens a file at the given path for reading.
    ///
    /// The returned [`File`] implements [`std::io::Read`].
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist or is not a regular file.
    pub fn open<P: AsRef<UnixPath>>(&self, path: P) -> Result<File<'_, I>> {
        let inode = self
            .get_path_inode(&path)?
            .ok_or_else(|| Error::PathNotFound(path.as_ref().to_string_lossy().into_owned()))?;

        self.open_inode_file(inode)
    }

    /// Opens a file from an inode directly.
    ///
    /// The inode must originate from this filesystem, for example from directory traversal.
    pub fn open_inode_file(&self, inode: Inode) -> Result<File<'_, I>> {
        if !inode.is_file() {
            return Err(Error::NotAFile(format!(
                "inode {} is not a regular file",
                inode.id()
            )));
        }

        Ok(File::new(inode, self))
    }

    /// Reads the target of a symbolic link inode from this filesystem without following it.
    pub fn read_link_inode(&self, inode: Inode) -> Result<UnixPathBuf> {
        if !inode.is_symlink() {
            return Err(Error::NotASymlink(inode.id()));
        }
        let size = inode.data_size();
        let mut target = Vec::new();
        for block_index in 0..size.div_ceil(self.core.block_size) {
            let block = self.get_inode_block(&inode, block_index * self.core.block_size)?;
            if block.contains(&0) {
                return Err(Error::CorruptedData(
                    "invalid symbolic link target".to_string(),
                ));
            }
            target
                .try_reserve(block.len())
                .map_err(|_| Error::OutOfBounds("symbolic link target is too large".to_string()))?;
            target.extend_from_slice(&block);
        }
        if target.is_empty() {
            return Err(Error::CorruptedData(
                "invalid symbolic link target".to_string(),
            ));
        }
        Ok(UnixPathBuf::from(target))
    }

    /// Reads all visible extended attributes without following symbolic links.
    /// Names and values are preserved as bytes; absent attributes yield an empty map.
    pub fn xattrs(&self, path: impl AsRef<UnixPath>) -> Result<Xattrs> {
        let inode = self
            .get_path_inode(path.as_ref())?
            .ok_or_else(|| Error::PathNotFound(path.as_ref().to_string_lossy().into_owned()))?;
        self.xattrs_inode(inode)
    }

    /// Reads extended attributes of an inode obtained from this filesystem.
    /// Attribute values are copied; file contents are not read unless long name
    /// prefixes reside in the special packed inode.
    pub fn xattrs_inode(&self, inode: Inode) -> Result<Xattrs> {
        let mut reader = XattrRead::new(&self.core, &inode)?;
        let packed = reader
            .packed_nid
            .map(|nid| self.get_inode(nid))
            .transpose()?;
        let mut cached: Cow<'_, [u8]> = Cow::Borrowed(&[]);
        let mut cached_offset = 0;
        while let Some(request) = reader.request() {
            let data = if request.packed {
                let inode = packed
                    .as_ref()
                    .ok_or_else(|| Error::CorruptedData("missing xattr prefix inode".into()))?;
                request.validate_inode(inode)?;
                if request.in_buffer(cached_offset, &cached).is_none() {
                    cached_offset = request.offset;
                    cached = self.get_inode_data(inode, request.offset)?;
                    if cached.len() < request.size {
                        cached = self.get_inode_block(inode, request.offset)?;
                    }
                }
                request.in_buffer(cached_offset, &cached)
            } else {
                self.image
                    .get(request.offset..request.offset + request.size as u64)
            }
            .ok_or_else(|| Error::OutOfBounds("failed to read xattr metadata".into()))?;
            reader.resume(data)?;
        }
        Ok(reader.finish())
    }

    /// Returns a reference to the filesystem superblock.
    pub fn super_block(&self) -> &SuperBlock {
        &self.core.super_block
    }

    pub fn get_inode(&self, nid: u64) -> Result<Inode> {
        let offset = self.core.get_inode_offset(nid)?;
        let compact_size = InodeCompact::size();
        let end = offset
            .checked_add(compact_size as u64)
            .ok_or(Error::Overflow("inode read range"))?;
        let data = self
            .image
            .get(offset..end)
            .ok_or_else(|| Error::OutOfBounds("failed to read inode format".to_string()))?;
        let (_, size) = EroFSCore::inode_header(data)?;
        let data = if size > compact_size {
            let end = offset
                .checked_add(size as u64)
                .ok_or(Error::Overflow("inode read range"))?;
            self.image
                .get(offset..end)
                .ok_or_else(|| Error::OutOfBounds("failed to read inode".to_string()))?
        } else {
            data
        };
        self.core.parse_inode(data, nid)
    }

    /// Assemble exactly one logical block, even when it crosses compressed extents.
    // Block consumers may decode an extent again; add iterator-local
    // caching if compressed directory traversal becomes a bottleneck.
    pub(crate) fn get_inode_block(&self, inode: &Inode, offset: u64) -> Result<Cow<'_, [u8]>> {
        let size = self.core.block_read_size(inode, offset)?;
        let mut data = self.get_inode_data(inode, offset)?;
        if data.len() >= size {
            match &mut data {
                Cow::Borrowed(bytes) => *bytes = &bytes[..size],
                Cow::Owned(bytes) => bytes.truncate(size),
            }
        } else {
            let buf = data.to_mut();
            buf.try_reserve_exact(size - buf.len())
                .map_err(|_| Error::OutOfBounds("cannot allocate inode block".to_string()))?;
            while buf.len() < size {
                let next = self.get_inode_data(inode, offset + buf.len() as u64)?;
                let n = next.len().min(size - buf.len());
                buf.extend_from_slice(&next[..n]);
            }
        }
        Ok(data)
    }

    pub(crate) fn get_inode_data(&self, inode: &Inode, offset: u64) -> Result<Cow<'_, [u8]>> {
        let mut plan = self.core.plan_inode_read(inode, offset)?;
        let mut limit = usize::MAX;
        loop {
            match plan {
                BlockPlan::Direct {
                    device_id,
                    offset,
                    size,
                } => {
                    let size = size.min(limit);
                    return self
                        .image_for_device(device_id)?
                        .get(offset..offset + size as u64)
                        .map(Cow::Borrowed)
                        .ok_or_else(|| Error::OutOfBounds("failed to get inode data".to_string()));
                }
                BlockPlan::Hole { size } => return Ok(Cow::Owned(vec![0; size.min(limit)])),
                BlockPlan::Fragment { offset, size } => {
                    let packed = self.get_inode(self.core.super_block.packed_nid)?;
                    plan = self.core.plan_fragment(&packed, offset, size)?;
                    limit = usize::try_from(size).unwrap_or(usize::MAX);
                }
                BlockPlan::Chunked {
                    addr_offset,
                    format,
                    block_index,
                    offset_in_block,
                    size,
                } => {
                    let entry_size = EroFSCore::chunk_entry_size(format);
                    let data = self
                        .image
                        .get(addr_offset..addr_offset + entry_size as u64)
                        .ok_or_else(|| Error::OutOfBounds("failed to get chunk index".into()))?;
                    plan = self.core.resolve_chunk_read(
                        data,
                        format,
                        block_index,
                        offset_in_block,
                        size,
                    )?;
                }
                BlockPlan::CompressionMetadata {
                    offset,
                    size,
                    reader,
                } => {
                    let data = self
                        .image
                        .get(offset..offset + size as u64)
                        .ok_or_else(|| {
                            Error::OutOfBounds("failed to read compression metadata".to_string())
                        })?;
                    plan = reader.resume(&self.core, offset, data)?;
                }
                BlockPlan::Encoded(extent) => {
                    let data = self
                        .image_for_device(extent.device_id)?
                        .get(extent.offset..extent.offset + extent.size as u64)
                        .ok_or_else(|| {
                            Error::OutOfBounds("failed to read compressed data".to_string())
                        })?;
                    return extent.decode(data, limit).map(Cow::Owned);
                }
            }
        }
    }

    pub(crate) fn get_path_inode<P: AsRef<UnixPath>>(&self, path: P) -> Result<Option<Inode>> {
        let mut nid = self.core.super_block.root_inode_id();

        let path = path.as_ref();
        'outer: for part in path
            .as_bytes()
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty())
        {
            let inode = self.get_inode(nid)?;
            if !inode.is_dir() {
                return Err(Error::NotADirectory(path.to_string_lossy().into_owned()));
            }
            if part == b"." {
                continue;
            }
            let block_count = inode.data_size().div_ceil(self.core.block_size);
            if block_count == 0 {
                return Ok(None);
            }

            for i in 0..block_count {
                let block = self.get_inode_block(&inode, i * self.core.block_size)?;
                if let Some(found_nid) = dirent::find_nodeid_by_name(part, &block)? {
                    nid = found_nid;
                    continue 'outer;
                }
            }
            return Ok(None);
        }

        let inode = self.get_inode(nid)?;
        if path.as_bytes().ends_with(b"/") && !inode.is_dir() {
            return Err(Error::NotADirectory(path.to_string_lossy().into_owned()));
        }
        Ok(Some(inode))
    }
}
