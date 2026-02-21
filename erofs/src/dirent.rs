use alloc::string::{String, ToString};
use core::{cmp, hint};

use binrw::{BinRead, io::Cursor};
use typed_path::{UnixPath, UnixPathBuf};

use crate::backend::Image;
use crate::{
    EroFS, Error, Result,
    types::{Dirent, DirentFileType, Inode},
};

pub fn find_nodeid_by_name(name: &[u8], data: &[u8]) -> Result<Option<u64>> {
    let dirent = read_nth_dirent(data, 0)?;
    let n = dirent.name_off as usize / Dirent::size();
    if n <= 2 {
        // Only "." and ".."
        return Ok(None);
    }

    let offset = 2;
    let mut size = n - offset;
    let mut base = 0usize;
    while size > 1 {
        let half = size / 2;
        let mid = base + half;

        let cmp = {
            let (_, entry_name) = read_nth_id_name(data, mid + offset, n)?;
            entry_name.cmp(name)
        };
        base = hint::select_unpredictable(cmp == cmp::Ordering::Greater, base, mid);

        size -= half;
    }

    let (inner_nid, cmp) = {
        let (nid, entry_name) = read_nth_id_name(data, base + offset, n)?;
        let cmp = entry_name.cmp(name);
        (nid, cmp)
    };
    if cmp != cmp::Ordering::Equal {
        return Ok(None);
    }

    Ok(Some(inner_nid))
}

fn read_nth_id_name(data: &[u8], n: usize, max: usize) -> Result<(u64, &[u8])> {
    let dirent = read_nth_dirent(data, n)?;
    let name_start = dirent.name_off as usize;
    let name_end = if n < max - 1 {
        let dirent = read_nth_dirent(data, n + 1)?;
        dirent.name_off as usize
    } else {
        data.len()
    };

    if name_end < name_start || name_end > data.len() {
        return Err(Error::CorruptedData(
            "invalid directory entry name offset".to_string(),
        ));
    }
    let name = &data[name_start..name_end];
    if let Some(i) = name.iter().position(|&b| b == 0) {
        // Trim trailing null bytes
        return Ok((dirent.nid, &name[..i]));
    }

    Ok((dirent.nid, name))
}

pub fn read_nth_dirent(data: &[u8], n: usize) -> Result<Dirent> {
    let start = n * Dirent::size();
    let slice = data
        .get(start..)
        .ok_or_else(|| Error::OutOfBounds("failed to get inode data".to_string()))?;
    let dirent = Dirent::read(&mut Cursor::new(slice))?;
    Ok(dirent)
}

#[derive(Debug)]
struct DirentBlock<'a> {
    data: &'a [u8],
    root: UnixPathBuf,
    dirent: Dirent,
    i: usize,
    n: usize,
}

impl<'a> DirentBlock<'a> {
    fn new(root: UnixPathBuf, data: &'a [u8]) -> Result<Self> {
        let dirent = read_nth_dirent(data, 0)?;
        let n = dirent.name_off as usize / Dirent::size();
        Ok(Self {
            root,
            data,
            dirent,
            i: 0,
            n,
        })
    }

    pub fn block_size(&self) -> usize {
        self.data.len()
    }

    fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        while self.i < self.n {
            let dirent = self.dirent;
            let name_start = dirent.name_off as usize;
            let name_end = if self.i < self.n - 1 {
                let dirent = read_nth_dirent(self.data, self.i + 1)?;
                self.dirent = dirent;
                dirent.name_off as usize
            } else {
                self.data.len()
            };

            if name_end < name_start || name_end > self.data.len() {
                return Err(Error::CorruptedData(
                    "invalid directory entry name offset".to_string(),
                ));
            }

            self.i += 1;
            let name: String = String::from_utf8_lossy(&self.data[name_start..name_end])
                .trim_end_matches('\0')
                .into();
            if name.as_str() == "." || name.as_str() == ".." {
                continue;
            }

            let entry = DirEntry {
                dir: self.root.clone(),
                nid: dirent.nid,
                file_type: dirent.file_type.try_into()?,
                file_name: name,
            };
            return Ok(Some(entry));
        }
        Ok(None)
    }
}

impl Iterator for DirentBlock<'_> {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.n {
            None
        } else {
            self.next_entry().transpose()
        }
    }
}

#[derive(Debug)]
pub struct ReadDir<'a, I: Image> {
    dir: UnixPathBuf,
    inode: Inode,
    erofs: &'a EroFS<I>,
    dirent_block: DirentBlock<'a>,
    offset: usize,
}

impl<'a, I: Image> ReadDir<'a, I> {
    pub(crate) fn new<P: AsRef<UnixPath>>(
        erofs: &'a EroFS<I>,
        inode: Inode,
        dir: P,
    ) -> Result<Self> {
        let block = erofs.get_inode_block(&inode, 0)?;
        let dirent_block = DirentBlock::new(dir.as_ref().to_path_buf(), block)?;
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
            inode,
            erofs,
            dirent_block,
            offset: 0,
        })
    }

    fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        if self.offset >= self.inode.data_size() {
            return Ok(None);
        }

        while self.offset < self.inode.data_size() {
            match self.dirent_block.next_entry()? {
                Some(entry) => return Ok(Some(entry)),
                None => {
                    self.offset += self.dirent_block.block_size();
                    if self.offset < self.inode.data_size() {
                        let block = self.erofs.get_inode_block(&self.inode, self.offset)?;
                        self.dirent_block = DirentBlock::new(self.dir.clone(), block)?;
                    }
                }
            }
        }
        Ok(None)
    }
}

impl<'a, I: Image> Iterator for ReadDir<'a, I> {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_entry().transpose()
    }
}

/// A directory entry within an EROFS filesystem.
#[derive(Debug, Clone)]
pub struct DirEntry {
    dir: UnixPathBuf,
    nid: u64,
    file_type: DirentFileType,
    file_name: String,
}

impl DirEntry {
    /// Returns the file type of this entry.
    pub fn file_type(&self) -> DirentFileType {
        self.file_type.clone()
    }

    /// Returns the file name of this entry.
    pub fn file_name(&self) -> String {
        self.file_name.clone()
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
