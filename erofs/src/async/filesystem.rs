use alloc::vec::Vec;
use alloc::{boxed::Box, format};
use bytes::BufMut;

use typed_path::{UnixPath, UnixPathBuf};

use super::file::File;
use super::walkdir::WalkDir;
use crate::backend::AsyncImage;
use crate::dirent;
use crate::filesystem::{BlockPlan, EroFSCore};
use crate::metadata::{self, ReadSource};
use crate::types::*;
use crate::xattr::XattrRead;
use crate::{DeviceInfo, Error, Result, Xattrs};

/// The async entry point for reading EROFS filesystem images.
///
/// `EroFS` provides async methods to traverse directories, open files, and access
/// filesystem metadata from EROFS images.
///
/// Paths are resolved component by component from the image root, including `.`
/// and `..`. Symbolic links are not followed; a trailing slash requires a directory.
#[derive(Debug, Clone)]
pub struct EroFS<I: AsyncImage> {
    image: I,
    devices: Vec<I>,
    core: EroFSCore,
}

impl<I: AsyncImage> EroFS<I> {
    /// Creates a new async `EroFS` instance from an async backend image source.
    ///
    /// Rejects images requiring unsupported incompatible features or additional images.
    pub async fn new(image: I) -> Result<Self> {
        Self::new_with_devices(image, Vec::new()).await
    }

    /// Opens an image with additional backing images in on-disk device-table order.
    /// The number of supplied images must match the active device table.
    pub async fn new_with_devices(image: I, devices: Vec<I>) -> Result<Self> {
        let mut super_block = [0; SuperBlock::size()];
        image
            .read_exact_at(&mut super_block, SUPER_BLOCK_OFFSET)
            .await?;
        let mut core = EroFSCore::new(&super_block)?;
        let extensions = core.extension_range()?;
        if !extensions.is_empty() {
            let mut data = vec![0; (extensions.end - extensions.start) as usize]; // At most 255 slots.
            image.read_exact_at(&mut data, extensions.start).await?;
            core.set_extensions(&data)?;
        }
        let range = core.device_table_range(devices.len())?;
        if !range.is_empty() {
            let size = (range.end - range.start) as usize; // At most 65535 device slots.
            let mut data = Vec::new();
            data.try_reserve_exact(size)
                .map_err(|_| Error::OutOfBounds("cannot allocate device table".into()))?;
            data.resize(size, 0);
            image.read_exact_at(&mut data, range.start).await?;
            core.set_device_table(&data)?;
        }
        Ok(Self {
            image,
            devices,
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
    pub async fn walk_dir(&self, root: impl AsRef<UnixPath>) -> Result<WalkDir<'_, I>> {
        WalkDir::new(self, root.as_ref()).await
    }

    /// Lists the immediate contents of a directory.
    pub async fn read_dir(&self, path: impl AsRef<UnixPath>) -> Result<WalkDir<'_, I>> {
        Ok(WalkDir::new(self, path.as_ref()).await?.max_depth(1))
    }

    /// Opens a file at the given path for reading.
    ///
    /// The returned [`File`] provides an async [`read`](File::read) method.
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist or is not a regular file.
    pub async fn open(&self, path: impl AsRef<UnixPath>) -> Result<File<'_, I>> {
        let inode = self
            .get_path_inode(path.as_ref())
            .await?
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
    /// The inode must originate from this filesystem. Target bytes are preserved;
    /// empty targets and targets containing NUL are rejected.
    pub async fn read_link_inode(&self, inode: Inode) -> Result<UnixPathBuf> {
        if !inode.is_symlink() {
            return Err(Error::NotASymlink(inode.id()));
        }
        let size = inode.data_size();
        let mut target = Vec::new();
        for block_index in 0..size.div_ceil(self.core.block_size) {
            let block = self
                .read_inode_block(&inode, block_index * self.core.block_size)
                .await?;
            if block.contains(&0) {
                return Err(Error::CorruptedData("invalid symbolic link target".into()));
            }
            target
                .try_reserve(block.len())
                .map_err(|_| Error::OutOfBounds("symbolic link target is too large".into()))?;
            target.put_slice(&block);
        }
        if target.is_empty() {
            return Err(Error::CorruptedData("invalid symbolic link target".into()));
        }
        Ok(UnixPathBuf::from(target))
    }

    /// Reads all visible extended attributes without following symbolic links.
    /// Names and values are preserved as bytes; absent attributes yield an empty map.
    pub async fn xattrs(&self, path: impl AsRef<UnixPath>) -> Result<Xattrs> {
        let inode = self
            .get_path_inode(path.as_ref())
            .await?
            .ok_or_else(|| Error::PathNotFound(path.as_ref().to_string_lossy().into_owned()))?;
        self.xattrs_inode(inode).await
    }

    /// Reads extended attributes of an inode obtained from this filesystem.
    /// Attribute values are copied without reading the subject's file data.
    /// Metadata may require decoding the metabox or special packed inode.
    pub async fn xattrs_inode(&self, inode: Inode) -> Result<Xattrs> {
        let mut reader = XattrRead::new(&self.core, &inode)?;
        let mut cached = Vec::new();
        let mut cached_offset = 0;
        let mut cached_source = ReadSource::Device(0);
        while let Some(request) = reader.request() {
            if let ReadSource::Inode(nid) = request.source {
                if cached_source != request.source
                    || request.in_buffer(cached_offset, &cached).is_none()
                {
                    let inode = self.source_inode(nid).await?;
                    cached = self
                        .read_inode_range(&inode, request.offset, request.size)
                        .await?;
                    cached_offset = request.offset;
                    cached_source = request.source;
                }
                let data = request
                    .in_buffer(cached_offset, &cached)
                    .ok_or_else(|| Error::OutOfBounds("failed to read xattr metadata".into()))?;
                reader.resume(data)?;
            } else {
                let mut data = vec![0; request.size];
                self.image.read_exact_at(&mut data, request.offset).await?;
                reader.resume(&data)?;
            }
        }
        Ok(reader.finish())
    }

    /// Returns a reference to the filesystem superblock.
    pub fn super_block(&self) -> &SuperBlock {
        &self.core.super_block
    }

    pub async fn get_inode(&self, nid: u64) -> Result<Inode> {
        let offset = self.core.get_inode_offset(nid)?;
        let source = self.core.metadata_source(nid)?;
        let compact_size = InodeCompact::size();
        let extended_offset = offset
            .checked_add(compact_size as u64)
            .ok_or(Error::Overflow("inode read range"))?;
        let mut buf = [0u8; InodeExtended::size()];
        self.read_source(source, &mut buf[..compact_size], offset)
            .await?;
        let (_, size) = EroFSCore::inode_header(&buf[..compact_size])?;
        if size > compact_size {
            offset
                .checked_add(size as u64)
                .ok_or(Error::Overflow("inode read range"))?;
            self.read_source(source, &mut buf[compact_size..size], extended_offset)
                .await?;
        }
        self.core.parse_inode(&buf[..size], nid)
    }

    async fn source_inode(&self, nid: u64) -> Result<Inode> {
        metadata::primary_inode(nid)?;
        // Boxing breaks the async type cycle, not the on-disk cycle: special
        // inode headers are required to reside in primary metadata.
        let inode = Box::pin(self.get_inode(nid)).await?;
        if !inode.is_file() {
            return Err(Error::CorruptedData(
                "metadata source is not a regular inode".into(),
            ));
        }
        Ok(inode)
    }

    async fn read_source(&self, source: ReadSource, buf: &mut [u8], offset: u64) -> Result<()> {
        match source {
            ReadSource::Device(device) => {
                self.image_for_device(device)?
                    .read_exact_at(buf, offset)
                    .await
            }
            ReadSource::Inode(nid) => {
                let inode = self.source_inode(nid).await?;
                let data = Box::pin(self.read_inode_range(&inode, offset, buf.len())).await?;
                buf.copy_from_slice(&data[..buf.len()]);
                Ok(())
            }
        }
    }

    // Retain coverage beyond the requested end for iterator-local metadata caches.
    async fn read_inode_range(&self, inode: &Inode, offset: u64, size: usize) -> Result<Vec<u8>> {
        metadata::validate_range(inode, offset, size)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut data = self.read_inode_data(inode, offset).await?;
        if data.len() < size {
            data.try_reserve_exact(size - data.len())
                .map_err(|_| Error::OutOfBounds("cannot allocate inode range".into()))?;
            while data.len() < size {
                let next = self
                    .read_inode_data(inode, offset + data.len() as u64)
                    .await?;
                if next.is_empty() {
                    return Err(Error::CorruptedData("inode read made no progress".into()));
                }
                let n = next.len().min(size - data.len());
                data.put_slice(&next[..n]);
            }
        }
        Ok(data)
    }

    pub(crate) async fn read_inode_block(&self, inode: &Inode, offset: u64) -> Result<Vec<u8>> {
        let size = self.core.block_read_size(inode, offset)?;
        let mut data = self.read_inode_range(inode, offset, size).await?;
        data.truncate(size);
        Ok(data)
    }

    pub(crate) async fn read_inode_data(&self, inode: &Inode, offset: u64) -> Result<Vec<u8>> {
        let mut plan = self.core.plan_inode_read(inode, offset)?;
        let mut limit = usize::MAX;
        loop {
            match plan {
                BlockPlan::Direct {
                    source,
                    offset,
                    size,
                } => {
                    let mut buf = vec![0u8; size.min(limit)];
                    self.read_source(source, &mut buf, offset).await?;
                    return Ok(buf);
                }
                BlockPlan::Hole { size } => return Ok(vec![0; size.min(limit)]),
                BlockPlan::Fragment { offset, size } => {
                    let packed = self.get_inode(self.core.super_block.packed_nid).await?;
                    plan = self.core.plan_fragment(&packed, offset, size)?;
                    limit = usize::try_from(size).unwrap_or(usize::MAX);
                }
                BlockPlan::Chunked {
                    source,
                    addr_offset,
                    format,
                    block_index,
                    offset_in_block,
                    size,
                } => {
                    let mut data = [0u8; 8];
                    let data = &mut data[..EroFSCore::chunk_entry_size(format)];
                    self.read_source(source, data, addr_offset).await?;
                    plan = self.core.resolve_chunk_read(
                        data,
                        format,
                        block_index,
                        offset_in_block,
                        size,
                    )?;
                }
                BlockPlan::CompressionMetadata {
                    source,
                    offset,
                    size,
                    reader,
                } => {
                    let mut data = vec![0; size];
                    self.read_source(source, &mut data, offset).await?;
                    plan = reader.resume(&self.core, offset, &data)?;
                }
                BlockPlan::Encoded(extent) => {
                    let mut data = vec![0; extent.size];
                    self.read_source(extent.source, &mut data, extent.offset)
                        .await?;
                    return extent.decode(&data, limit);
                }
            }
        }
    }

    pub(crate) async fn get_path_inode(&self, path: &UnixPath) -> Result<Option<Inode>> {
        let mut nid = self.core.super_block.root_inode_id();

        'outer: for part in path
            .as_bytes()
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty())
        {
            let inode = self.get_inode(nid).await?;
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
                let block = self
                    .read_inode_block(&inode, i * self.core.block_size)
                    .await?;
                if let Some(found_nid) = dirent::find_nodeid_by_name(part, &block)? {
                    nid = found_nid;
                    continue 'outer;
                }
            }
            return Ok(None);
        }

        let inode = self.get_inode(nid).await?;
        if path.as_bytes().ends_with(b"/") && !inode.is_dir() {
            return Err(Error::NotADirectory(path.to_string_lossy().into_owned()));
        }
        Ok(Some(inode))
    }
}
