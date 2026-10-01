use alloc::format;
use alloc::vec::Vec;

use typed_path::UnixPath;

use super::file::File;
use super::walkdir::WalkDir;
use crate::backend::AsyncImage;
use crate::dirent;
use crate::filesystem::{BlockPlan, EroFSCore};
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
    /// Attribute values are copied; file contents are not read unless long name
    /// prefixes reside in the special packed inode.
    pub async fn xattrs_inode(&self, inode: Inode) -> Result<Xattrs> {
        let mut reader = XattrRead::new(&self.core, &inode)?;
        let packed = match reader.packed_nid {
            Some(nid) => Some(self.get_inode(nid).await?),
            None => None,
        };
        let mut cached = Vec::new();
        let mut cached_offset = 0;
        while let Some(request) = reader.request() {
            if request.packed {
                let inode = packed
                    .as_ref()
                    .ok_or_else(|| Error::CorruptedData("missing xattr prefix inode".into()))?;
                request.validate_inode(inode)?;
                if request.in_buffer(cached_offset, &cached).is_none() {
                    cached_offset = request.offset;
                    cached = self.read_inode_data(inode, request.offset).await?;
                    if cached.len() < request.size {
                        cached = self.read_inode_block(inode, request.offset).await?;
                    }
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
        let compact_size = InodeCompact::size();
        let extended_offset = offset
            .checked_add(compact_size as u64)
            .ok_or(Error::Overflow("inode read range"))?;
        let mut buf = [0u8; InodeExtended::size()];
        self.image
            .read_exact_at(&mut buf[..compact_size], offset)
            .await?;
        let (_, size) = EroFSCore::inode_header(&buf[..compact_size])?;
        if size > compact_size {
            offset
                .checked_add(size as u64)
                .ok_or(Error::Overflow("inode read range"))?;
            self.image
                .read_exact_at(&mut buf[compact_size..size], extended_offset)
                .await?;
        }
        self.core.parse_inode(&buf[..size], nid)
    }

    pub(crate) async fn read_inode_block(&self, inode: &Inode, offset: u64) -> Result<Vec<u8>> {
        let size = self.core.block_read_size(inode, offset)?;
        let mut data = self.read_inode_data(inode, offset).await?;
        data.truncate(size);
        data.try_reserve_exact(size - data.len())
            .map_err(|_| Error::OutOfBounds("cannot allocate inode block".into()))?;
        while data.len() < size {
            let next = self
                .read_inode_data(inode, offset + data.len() as u64)
                .await?;
            let n = next.len().min(size - data.len());
            data.extend_from_slice(&next[..n]);
        }
        Ok(data)
    }

    pub(crate) async fn read_inode_data(&self, inode: &Inode, offset: u64) -> Result<Vec<u8>> {
        let mut plan = self.core.plan_inode_read(inode, offset)?;
        let mut limit = usize::MAX;
        loop {
            match plan {
                BlockPlan::Direct {
                    device_id,
                    offset,
                    size,
                } => {
                    let mut buf = vec![0u8; size.min(limit)];
                    self.image_for_device(device_id)?
                        .read_exact_at(&mut buf, offset)
                        .await?;
                    return Ok(buf);
                }
                BlockPlan::Hole { size } => return Ok(vec![0; size.min(limit)]),
                BlockPlan::Fragment { offset, size } => {
                    let packed = self.get_inode(self.core.super_block.packed_nid).await?;
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
                    let mut data = [0u8; 8];
                    let data = &mut data[..EroFSCore::chunk_entry_size(format)];
                    self.image.read_exact_at(data, addr_offset).await?;
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
                    let mut data = vec![0; size];
                    self.image.read_exact_at(&mut data, offset).await?;
                    plan = reader.resume(&self.core, offset, &data)?;
                }
                BlockPlan::Encoded(extent) => {
                    let mut data = vec![0; extent.size];
                    self.image_for_device(extent.device_id)?
                        .read_exact_at(&mut data, extent.offset)
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
