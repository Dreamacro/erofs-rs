use alloc::{string::ToString, vec::Vec};
use core::cmp::Ordering;

use binrw::{BinRead, io::Cursor};
use typed_path::UnixPathBuf;

use crate::{
    Error, Result,
    types::{Dirent, DirentFileType},
};

pub fn find_nodeid_by_name(name: &[u8], data: &[u8]) -> Result<Option<u64>> {
    let count = validate_dirent_block(data)?;
    let mut left = 0;
    let mut right = count;

    while left < right {
        let mid = left + (right - left) / 2;
        let (entry, entry_name) = read_entry(data, mid, count)?;
        match entry_name.cmp(name) {
            Ordering::Less => left = mid + 1,
            Ordering::Greater => right = mid,
            Ordering::Equal => return Ok(Some(entry.nid)),
        }
    }
    Ok(None)
}

/// Scan the whole block so corruption outside the binary-search path is not hidden.
fn validate_dirent_block(data: &[u8]) -> Result<usize> {
    let table_size = usize::from(read_nth_dirent(data, 0)?.name_off);
    if table_size == 0 || table_size >= data.len() || !table_size.is_multiple_of(Dirent::size()) {
        return Err(Error::CorruptedData(
            "invalid directory entry table size".to_string(),
        ));
    }

    let count = table_size / Dirent::size();
    let mut previous_name: &[u8] = &[];
    for index in 0..count {
        let (_, name) = read_entry(data, index, count)?;
        if name <= previous_name {
            return Err(Error::CorruptedData(
                "directory entries are not strictly sorted".to_string(),
            ));
        }
        previous_name = name;
    }
    Ok(count)
}

fn read_entry(data: &[u8], index: usize, count: usize) -> Result<(Dirent, &[u8])> {
    if index >= count {
        return Err(Error::OutOfRange(index, count));
    }
    let dirent = read_nth_dirent(data, index)?;
    let name_start = usize::from(dirent.name_off);
    let is_last = index == count - 1;
    let name_end = if is_last {
        data.len()
    } else {
        usize::from(read_nth_dirent(data, index + 1)?.name_off)
    };

    if name_start < count * Dirent::size() || name_start >= name_end || name_end > data.len() {
        return Err(Error::CorruptedData(
            "invalid directory entry name offset".to_string(),
        ));
    }
    let mut name = &data[name_start..name_end];
    if is_last {
        // Only the last name can be followed by NUL-terminated block padding.
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        name = &name[..len];
    }
    if name.is_empty() || name.len() > 255 || name.contains(&0) || name.contains(&b'/') {
        return Err(Error::CorruptedData(
            "invalid directory entry name".to_string(),
        ));
    }
    DirentFileType::try_from(dirent.file_type)?;
    Ok((dirent, name))
}

fn read_nth_dirent(data: &[u8], n: usize) -> Result<Dirent> {
    let (entries, _) = data.as_chunks::<{ Dirent::size() }>();
    let slice = entries
        .get(n)
        .ok_or_else(|| Error::CorruptedData("truncated directory entry".to_string()))?;
    Ok(Dirent::read(&mut Cursor::new(slice))?)
}

#[derive(Debug)]
pub struct DirentBlock<D: AsRef<[u8]>> {
    data: D,
    root: UnixPathBuf,
    i: usize,
    n: usize,
}

impl<D: AsRef<[u8]>> DirentBlock<D> {
    pub(crate) fn new(root: UnixPathBuf, data: D) -> Result<Self> {
        let n = validate_dirent_block(data.as_ref())?;
        Ok(Self {
            root,
            data,
            i: 0,
            n,
        })
    }

    pub(crate) fn block_size(&self) -> usize {
        self.data.as_ref().len()
    }

    pub(crate) fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        let data = self.data.as_ref();
        while self.i < self.n {
            let index = self.i;
            self.i += 1;
            let (dirent, name) = read_entry(data, index, self.n)?;
            if name == b"." || name == b".." {
                continue;
            }

            let entry = DirEntry {
                dir: self.root.clone(),
                nid: dirent.nid,
                file_type: dirent.file_type.try_into()?,
                file_name: name.to_vec(),
            };
            return Ok(Some(entry));
        }
        Ok(None)
    }
}

impl<D: AsRef<[u8]>> Iterator for DirentBlock<D> {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_entry().transpose()
    }
}

/// A directory entry within an EROFS filesystem.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub(crate) dir: UnixPathBuf,
    pub(crate) nid: u64,
    pub(crate) file_type: DirentFileType,
    pub(crate) file_name: Vec<u8>,
}

impl DirEntry {
    /// Returns the file type of this entry.
    pub fn file_type(&self) -> DirentFileType {
        self.file_type
    }

    /// Returns the file name of this entry as raw bytes.
    ///
    /// EROFS file names are not required to be valid UTF-8.
    pub fn file_name(&self) -> &[u8] {
        &self.file_name
    }

    /// Returns the full path of this entry.
    pub fn path(&self) -> UnixPathBuf {
        self.dir.join(&self.file_name)
    }

    /// Returns the node ID (inode number) of this entry.
    pub fn nid(&self) -> u64 {
        self.nid
    }
}
