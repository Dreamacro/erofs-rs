//! Shared extended-attribute parsing; source I/O stays in the executors.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    format, vec,
    vec::Vec,
};
use binrw::{BinRead, io::Cursor};
use bytes::{Buf, BufMut};

use crate::{
    Error, Result,
    filesystem::EroFSCore,
    metadata::ReadSource,
    types::{Inode, XattrEntry, XattrHeader},
};

#[cfg(test)]
mod tests;

/// Extended attributes indexed by their full, lossless byte names.
///
/// Values are uninterpreted bytes, including ACLs and security attributes.
/// Internal attributes in namespace zero are not exposed. No host permission
/// checks are applied when reading attributes from an image.
pub type Xattrs = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Clone, Copy)]
pub struct Request {
    pub offset: u64,
    pub size: usize,
    pub source: ReadSource,
}

impl Request {
    fn new(offset: u64, size: usize, source: ReadSource) -> Result<Self> {
        offset
            .checked_add(size as u64)
            .ok_or(Error::Overflow("xattr read range"))?;
        Ok(Self {
            offset,
            size,
            source,
        })
    }

    pub fn in_buffer(self, offset: u64, data: &[u8]) -> Option<&[u8]> {
        let start = usize::try_from(self.offset.checked_sub(offset)?).ok()?;
        data.get(start..start.checked_add(self.size)?)
    }
}

#[derive(Clone, Copy)]
enum Stage {
    PrefixLength,
    Prefix,
    Body,
    SharedHeader,
    SharedValue(XattrEntry),
}

pub struct XattrRead {
    request: Option<Request>,
    stage: Stage,
    body: Request,
    prefix_count: usize,
    prefixes: Vec<Vec<u8>>,
    shared: vec::IntoIter<u32>,
    shared_base: u64,
    shared_source: Option<ReadSource>,
    attrs: Xattrs,
}

impl XattrRead {
    pub fn new(core: &EroFSCore, inode: &Inode) -> Result<Self> {
        let size = inode.xattr_size();
        let offset = if size == 0 {
            0
        } else {
            core.get_inode_offset(inode.id())?
                .checked_add(inode.inode_size as u64)
                .ok_or(Error::Overflow("inode xattr offset"))?
        };
        let body = Request::new(offset, size, core.metadata_source(inode.id())?)?;
        let sb = &core.super_block;
        let mut reader = Self {
            request: (size != 0).then_some(body),
            stage: Stage::Body,
            body,
            prefix_count: usize::from(sb.xattr_prefix_count),
            prefixes: Vec::new(),
            shared: Vec::new().into_iter(),
            shared_base: core.block_offset(sb.xattr_blk_addr),
            shared_source: if sb.feature_compat & 8 != 0 {
                core.metabox_nid.map(ReadSource::Inode)
            } else {
                Some(ReadSource::Device(0))
            },
            attrs: Xattrs::new(),
        };
        if size == 0 {
            return Ok(reader);
        }
        if size == size_of::<XattrHeader>() {
            return Err(Error::NotSupported("header-only xattr body".into()));
        }
        if reader.prefix_count != 0 {
            if reader.prefix_count > 128 || sb.feature_incompat & 0x40 == 0 {
                return Err(Error::CorruptedData(
                    "invalid xattr prefix count or feature".into(),
                ));
            }
            let source = if sb.feature_compat & 0x10 != 0 {
                ReadSource::Device(0)
            } else {
                core.metabox_nid
                    .filter(|&nid| nid != 0)
                    .or_else(|| (sb.packed_nid != 0).then_some(sb.packed_nid))
                    .map_or(ReadSource::Device(0), ReadSource::Inode)
            };
            reader.stage = Stage::PrefixLength;
            reader.request = Some(Request::new(
                u64::from(sb.xattr_prefix_start) * 4,
                2,
                source,
            )?);
        }
        Ok(reader)
    }

    pub fn request(&self) -> Option<Request> {
        self.request
    }

    pub fn finish(self) -> Xattrs {
        self.attrs
    }

    pub fn resume(&mut self, data: &[u8]) -> Result<()> {
        let request = self
            .request
            .ok_or_else(|| Error::CorruptedData("unexpected xattr data".into()))?;
        if data.len() != request.size {
            return Err(Error::CorruptedData("truncated xattr metadata".into()));
        }
        match self.stage {
            Stage::PrefixLength => {
                let length = usize::from((&data[..2]).get_u16_le());
                if !(1..=256).contains(&length) {
                    return Err(Error::CorruptedData("invalid xattr prefix length".into()));
                }
                self.stage = Stage::Prefix;
                self.request = Some(Request::new(request.offset + 2, length, request.source)?);
            }
            Stage::Prefix => {
                // Validate the short namespace now; zero denotes hidden metadata.
                let base = namespace(data[0])?;
                if data[1..].contains(&0)
                    || base.len() + data.len() - 1 > 255
                    || (matches!(data[0], 2 | 3) && data.len() != 1)
                {
                    return Err(Error::CorruptedData("invalid xattr name prefix".into()));
                }
                self.prefixes.push(data.to_vec());
                if self.prefixes.len() == self.prefix_count {
                    self.stage = Stage::Body;
                    self.request = Some(self.body);
                } else {
                    let offset = request
                        .offset
                        .checked_add(request.size as u64)
                        .and_then(|end| end.checked_add(3))
                        .ok_or(Error::Overflow("xattr prefix offset"))?
                        & !3;
                    self.stage = Stage::PrefixLength;
                    self.request = Some(Request::new(offset, 2, request.source)?);
                }
            }
            Stage::Body => {
                let header = XattrHeader::read(&mut Cursor::new(data))?;
                if header.reserved != [0; 7] {
                    return Err(Error::NotSupported("xattr header extensions".into()));
                }
                // The name filter is an optional lookup accelerator, not entry data.
                let start = size_of::<XattrHeader>();
                let end = start + usize::from(header.shared_count) * 4;
                let shared = data.get(start..end).ok_or_else(|| {
                    Error::CorruptedData("xattr shared IDs exceed inode body".into())
                })?;
                if header.shared_count != 0 && self.shared_source.is_none() {
                    return Err(Error::CorruptedData("shared xattrs without metabox".into()));
                }
                self.shared = shared
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|id| u32::from_le_bytes(*id))
                    .collect::<Vec<_>>()
                    .into_iter();
                let mut entries = &data[end..];
                while !entries.is_empty() {
                    let entry = XattrEntry::read(&mut Cursor::new(entries))?;
                    let length = usize::from(entry.name_len) + usize::from(entry.value_len);
                    let size = (size_of::<XattrEntry>() + length).next_multiple_of(4);
                    let record = entries.get(..size).ok_or_else(|| {
                        Error::CorruptedData("xattr entry exceeds inode body".into())
                    })?;
                    self.insert(
                        entry,
                        &record[size_of::<XattrEntry>()..size_of::<XattrEntry>() + length],
                    )?;
                    entries.advance(size);
                }
                self.next_shared()?;
            }
            Stage::SharedHeader => {
                let entry = XattrEntry::read(&mut Cursor::new(data))?;
                let size = usize::from(entry.name_len) + usize::from(entry.value_len);
                if size == 0 {
                    self.insert(entry, &[])?;
                    self.next_shared()?;
                } else {
                    self.stage = Stage::SharedValue(entry);
                    self.request = Some(Request::new(request.offset + 4, size, request.source)?);
                }
            }
            Stage::SharedValue(entry) => {
                self.insert(entry, data)?;
                self.next_shared()?;
            }
        }
        Ok(())
    }

    fn next_shared(&mut self) -> Result<()> {
        self.stage = Stage::SharedHeader;
        self.request = self
            .shared
            .next()
            .map(|id| {
                let offset = self
                    .shared_base
                    .checked_add(u64::from(id) * 4)
                    .ok_or(Error::Overflow("shared xattr offset"))?;
                let source = self
                    .shared_source
                    .ok_or_else(|| Error::CorruptedData("shared xattrs without metabox".into()))?;
                Request::new(offset, size_of::<XattrEntry>(), source)
            })
            .transpose()?;
        Ok(())
    }

    fn insert(&mut self, entry: XattrEntry, data: &[u8]) -> Result<()> {
        let (index, infix) = if entry.name_index & 0x80 != 0 {
            let prefix = self
                .prefixes
                .get(usize::from(entry.name_index & 0x7f))
                .ok_or_else(|| Error::CorruptedData("xattr prefix index outside table".into()))?;
            (prefix[0], &prefix[1..])
        } else {
            (entry.name_index, &[][..])
        };
        let prefix = namespace(index)?;
        // Namespace zero contains internal attributes and zero-filled body padding.
        if index == 0 {
            return Ok(());
        }
        let (suffix, value) = data.split_at(usize::from(entry.name_len));
        let suffix_len = infix.len() + suffix.len();
        if prefix.len() + suffix_len > 255
            || suffix.contains(&0)
            || if matches!(index, 2 | 3) {
                suffix_len != 0
            } else {
                suffix_len == 0
            }
        {
            return Err(Error::CorruptedData("invalid xattr name".into()));
        }
        let name = [prefix, infix, suffix].concat();
        match self.attrs.entry(name) {
            Entry::Occupied(_) => Err(Error::CorruptedData("duplicate xattr name".into())),
            Entry::Vacant(entry) => {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(value.len())
                    .map_err(|_| Error::OutOfBounds("cannot allocate xattr value".into()))?;
                bytes.put_slice(value);
                entry.insert(bytes);
                Ok(())
            }
        }
    }
}

fn namespace(index: u8) -> Result<&'static [u8]> {
    match index {
        0 => Ok(b""),
        1 => Ok(b"user."),
        2 => Ok(b"system.posix_acl_access"),
        3 => Ok(b"system.posix_acl_default"),
        4 => Ok(b"trusted."),
        6 => Ok(b"security."),
        _ => Err(Error::NotSupported(format!("xattr namespace {index}"))),
    }
}
