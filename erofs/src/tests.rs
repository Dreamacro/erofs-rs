use alloc::vec::Vec;
use bytes::BufMut;
use core::{
    future::Future,
    ops::{Bound::*, RangeBounds},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    task::{Context, Poll, Waker},
};

#[cfg(not(feature = "std"))]
pub use crate::sync::file::Read;
use crate::{
    Error, Result,
    backend::{AsyncImage, Image, SliceImage},
    types::MAGIC_NUMBER,
};
#[cfg(feature = "std")]
pub use std::io::Read;

pub struct Source<'a> {
    pub data: SliceImage<'a>,
    pub reads: AtomicUsize,
    pub fail: AtomicBool,
}

impl Image for &Source<'_> {
    fn len(&self) -> u64 {
        self.data.len()
    }
    fn get<R: RangeBounds<u64>>(&self, range: R) -> Option<&[u8]> {
        self.reads.fetch_add(1, Relaxed);
        if self.fail.load(Relaxed) {
            return None;
        }
        self.data.get(range)
    }
}

impl AsyncImage for &Source<'_> {
    async fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::Overflow("test read"))?;
        let bytes = self
            .get(offset..end)
            .ok_or_else(|| Error::OutOfBounds("injected read failure".into()))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

pub fn ready<T>(future: impl Future<Output = T>) -> T {
    match core::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("in-memory backend must complete immediately"),
    }
}

pub fn directory(data: &mut [u8], entries: &[(u64, &[u8], u8)]) {
    data.fill(0);
    let mut name_offset = entries.len() * 12;
    for (index, &(nid, name, kind)) in entries.iter().enumerate() {
        let at = index * 12;
        let mut fields = &mut data[at..];
        fields.put_u64_le(nid);
        fields.put_u16_le(name_offset as u16);
        fields.put_u8(kind);
        data[name_offset..name_offset + name.len()].copy_from_slice(name);
        name_offset += name.len();
    }
}

pub fn image() -> Vec<u8> {
    let mut data = vec![0; 6656];
    (&mut data[1024..]).put_u32_le(MAGIC_NUMBER);
    data[1036] = 9;
    data[1038] = 1;
    data[1064] = 4;
    for (nid, mode, size, block) in [
        (1, 0o40755u16, 1024u32, 8u32),
        (2, 0o100644, 700, 10),
        (3, 0o100644, 1, 12),
    ] {
        let at = 2048 + nid * 32;
        (&mut data[at + 4..]).put_u16_le(mode);
        (&mut data[at + 8..]).put_u32_le(size);
        (&mut data[at + 16..]).put_u32_le(block);
    }
    // Dot inodes must not be loaded. Advisory dtype deliberately disagrees with inode type.
    directory(
        &mut data[4096..4608],
        &[(u64::MAX, b".", 2), (u64::MAX, b"..", 2), (2, b"a", 2)],
    );
    directory(&mut data[4608..5120], &[(3, b"b", 0)]);
    data[5120..5632].fill(b'A');
    data[5632..5820].fill(b'B');
    data[6144] = b'C';
    data
}

#[test]
fn image_ranges_are_checked_without_narrowing_or_overflow() {
    fn check(image: &impl Image) {
        assert_eq!(image.len(), 5);
        for (range, expected) in [
            ((Unbounded, Unbounded), Some(&b"abcde"[..])),
            ((Included(1), Included(3)), Some(&b"bcd"[..])),
            ((Excluded(1), Excluded(4)), Some(&b"cd"[..])),
            ((Included(5), Unbounded), Some(&b""[..])),
            ((Included(4), Excluded(3)), None),
            ((Included(6), Unbounded), None),
            ((Excluded(u64::MAX), Unbounded), None),
            ((Unbounded, Included(u64::MAX)), None),
            ((Included(1 << 32), Excluded((1 << 32) + 1)), None),
        ] {
            assert_eq!(image.get(range), expected);
        }
    }
    check(&SliceImage::new(b"abcde"));
    assert_eq!(SliceImage::new(b"").get(..), Some(&b""[..]));
    #[cfg(feature = "std")]
    {
        let mut map = memmap2::MmapMut::map_anon(5).unwrap();
        map.copy_from_slice(b"abcde");
        check(&crate::backend::MmapImage::new(
            map.make_read_only().unwrap(),
        ));
    }
}

#[test]
fn directory_block_failures_do_not_skip_entries() {
    let mut data = image();
    for corrupt in [false, true] {
        if corrupt {
            data[4616..4618].fill(0); // Invalid name-table boundary in the second block.
        }
        let source = Source {
            data: SliceImage::new(&data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        let mut dir = fs.read_dir("/").unwrap();
        let mut adir = ready(afs.read_dir("/")).unwrap();
        assert_eq!(dir.next().unwrap().unwrap().dir_entry.file_name(), b"a");
        assert_eq!(
            ready(adir.next_entry())
                .unwrap()
                .unwrap()
                .dir_entry
                .file_name(),
            b"a"
        );
        source.fail.store(true, Relaxed);
        assert!(dir.next().unwrap().is_err());
        assert!(ready(adir.next_entry()).unwrap().is_err());
        source.fail.store(false, Relaxed);
        if corrupt {
            for _ in 0..2 {
                assert!(matches!(dir.next().unwrap(), Err(Error::CorruptedData(_))));
                assert!(matches!(
                    ready(adir.next_entry()).unwrap(),
                    Err(Error::CorruptedData(_))
                ));
            }
        } else {
            assert_eq!(dir.next().unwrap().unwrap().dir_entry.file_name(), b"b");
            assert_eq!(
                ready(adir.next_entry())
                    .unwrap()
                    .unwrap()
                    .dir_entry
                    .file_name(),
                b"b"
            );
            assert!(dir.next().is_none());
            assert!(ready(adir.next_entry()).is_none());
        }
    }
}

#[test]
fn walking_uses_inode_types_and_rejects_ancestor_cycles() {
    let mut data = image();
    data[4130] = 1; // A directory advertised as a regular file.
    data[2112] = 0x10; // Directory with omitted dot entry.
    (&mut data[2116..]).put_u16_le(0o40755);
    (&mut data[2120..]).put_u32_le(512);
    directory(&mut data[5120..5632], &[(1, b"back", 1), (3, b"\xff", 0)]);
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let mut dir = fs.walk_dir("/").unwrap();
    let mut adir = ready(afs.walk_dir("/")).unwrap();
    for (name, depth) in [(b"a".as_slice(), 1), (b"\xff", 2), (b"b", 1)] {
        let entry = dir.next().unwrap().unwrap();
        let aentry = ready(adir.next_entry()).unwrap().unwrap();
        assert_eq!((entry.dir_entry.file_name(), entry.depth), (name, depth));
        assert_eq!((aentry.dir_entry.file_name(), aentry.depth), (name, depth));
        if name == b"a" {
            assert!(entry.inode.is_dir());
            assert!(matches!(dir.next().unwrap(), Err(Error::CorruptedData(_))));
            assert!(matches!(
                ready(adir.next_entry()).unwrap(),
                Err(Error::CorruptedData(_))
            ));
        }
    }
    assert!(dir.next().is_none());
    assert!(ready(adir.next_entry()).is_none());
    let mut dir = fs.read_dir("/a/.").unwrap();
    let mut adir = ready(afs.read_dir("/a/.")).unwrap();
    for name in [b"back".as_slice(), b"\xff"] {
        let entry = dir.next().unwrap().unwrap();
        let aentry = ready(adir.next_entry()).unwrap().unwrap();
        assert_eq!((entry.dir_entry.file_name(), entry.depth), (name, 1));
        assert_eq!((aentry.dir_entry.file_name(), aentry.depth), (name, 1));
    }
    assert!(dir.next().is_none());
    assert!(ready(adir.next_entry()).is_none());
}

#[test]
fn plain_files_preserve_paths_cache_and_read_positions() {
    let data = image();
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    for path in ["/a/.", "/a/", "/a/../b"] {
        assert!(matches!(fs.open(path), Err(Error::NotADirectory(_))));
        assert!(matches!(
            ready(afs.open(path)),
            Err(Error::NotADirectory(_))
        ));
    }
    assert!(matches!(
        fs.open("/missing/../a"),
        Err(Error::PathNotFound(_))
    ));
    assert!(matches!(
        ready(afs.open("/missing/../a")),
        Err(Error::PathNotFound(_))
    ));
    let inode = fs.get_inode(2).unwrap();
    assert!(matches!(
        fs.get_inode_data(&inode, 0).unwrap(),
        alloc::borrow::Cow::Borrowed(_)
    ));
    let mut file = fs.open("//./a").unwrap();
    let mut afile = ready(afs.open("//./a")).unwrap();
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    assert_eq!(file.read(&mut []).unwrap(), 0);
    assert_eq!(ready(afile.read(&mut [])).unwrap(), 0);
    assert_eq!(source.reads.load(Relaxed), reads);
    let mut buf = [0; 600];
    assert!(file.read(&mut buf).is_err());
    assert!(ready(afile.read(&mut buf)).is_err());
    source.fail.store(false, Relaxed);
    assert_eq!(file.read(&mut buf[..17]).unwrap(), 17);
    assert_eq!(&buf[..17], &[b'A'; 17]);
    assert_eq!(ready(afile.read(&mut buf[..17])).unwrap(), 17);
    assert_eq!(&buf[..17], &[b'A'; 17]);
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    assert_eq!(file.read(&mut buf).unwrap(), 495);
    assert_eq!(&buf[..495], &[b'A'; 495]);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 495);
    assert_eq!(&buf[..495], &[b'A'; 495]);
    assert_eq!(source.reads.load(Relaxed), reads);
    assert!(file.read(&mut buf).is_err());
    assert!(ready(afile.read(&mut buf)).is_err());
    source.fail.store(false, Relaxed);
    assert_eq!(file.read(&mut buf).unwrap(), 188);
    assert_eq!(&buf[..188], &[b'B'; 188]);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 188);
    assert_eq!(&buf[..188], &[b'B'; 188]);
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    assert_eq!(file.read(&mut buf).unwrap(), 0);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 0);
    assert_eq!(source.reads.load(Relaxed), reads);
}

pub fn check_read_at<I: Image, A: AsyncImage>(
    file: &crate::sync::file::File<'_, I>,
    afile: &crate::r#async::file::File<'_, A>,
    expected: &[u8],
) {
    let size = expected.len() as u64;
    for offset in [
        size,
        size.saturating_sub(1),
        511,
        0,
        size / 2,
        513,
        u64::MAX,
    ] {
        for asynchronous in [false, true] {
            let mut buf = [0xa5; 113];
            let n = if asynchronous {
                ready(afile.read_at(&mut buf, offset)).unwrap()
            } else {
                file.read_at(&mut buf, offset).unwrap()
            };
            let at = offset.min(size) as usize;
            assert!(n <= buf.len().min(expected.len() - at));
            assert_eq!(n == 0, offset >= size);
            assert_eq!(&buf[..n], &expected[at..at + n]);
            assert!(buf[n..].iter().all(|&byte| byte == 0xa5));
        }
    }
}

#[test]
fn positioned_reads_preserve_sequential_cache_and_retry_state() {
    let mut data = image();
    let expected: Vec<u8> = (0..700).map(|n| (n % 251) as u8).collect();
    data[5120..5820].copy_from_slice(&expected);
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let mut file = fs.open("/a").unwrap();
    let mut afile = ready(afs.open("/a")).unwrap();
    let mut buf = [0; 7];
    assert_eq!(file.read(&mut buf).unwrap(), 7);
    assert_eq!(buf, expected[..7]);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 7);
    assert_eq!(buf, expected[..7]);
    check_read_at(&file, &afile, &expected);

    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    for offset in [0, 700, 1 << 40, u64::MAX] {
        assert_eq!(file.read_at(&mut [], offset).unwrap(), 0);
        assert_eq!(ready(afile.read_at(&mut [], offset)).unwrap(), 0);
        if offset >= 700 {
            assert_eq!(file.read_at(&mut buf, offset).unwrap(), 0);
            assert_eq!(ready(afile.read_at(&mut buf, offset)).unwrap(), 0);
        }
    }
    assert_eq!(source.reads.load(Relaxed), reads);
    buf.fill(0xa5);
    assert!(file.read_at(&mut buf, 513).is_err());
    assert_eq!(buf, [0xa5; 7]);
    assert!(ready(afile.read_at(&mut buf, 513)).is_err());
    assert_eq!(buf, [0xa5; 7]);
    let reads = source.reads.load(Relaxed);
    assert_eq!(file.read(&mut buf).unwrap(), 7);
    assert_eq!(buf, expected[7..14]);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 7);
    assert_eq!(buf, expected[7..14]);
    assert_eq!(source.reads.load(Relaxed), reads);
    source.fail.store(false, Relaxed);
    assert_eq!(file.read_at(&mut buf, 513).unwrap(), 7);
    assert_eq!(buf, expected[513..520]);
    assert_eq!(ready(afile.read_at(&mut buf, 513)).unwrap(), 7);
    assert_eq!(buf, expected[513..520]);
    assert_eq!(file.read(&mut buf).unwrap(), 7);
    assert_eq!(buf, expected[14..21]);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 7);
    assert_eq!(buf, expected[14..21]);
}

#[test]
fn positioned_reads_keep_u64_offsets() {
    let mut data = image();
    data[2112..2176].fill(0);
    data[2112] = 1; // Extended flat inode, entirely sparse.
    (&mut data[2116..]).put_u16_le(0o100644);
    (&mut data[2120..]).put_u64_le(u64::MAX);
    (&mut data[2128..]).put_u32_le(u32::MAX);
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let file = fs.open_inode_file(fs.get_inode(2).unwrap()).unwrap();
    let afile = afs
        .open_inode_file(ready(afs.get_inode(2)).unwrap())
        .unwrap();
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    for (offset, count) in [
        (1 << 32, 7),
        ((1 << 48) + 17, 7),
        (u64::MAX - 1, 1),
        (u64::MAX, 0),
    ] {
        for asynchronous in [false, true] {
            let mut buf = [0xa5; 7];
            let n = if asynchronous {
                ready(afile.read_at(&mut buf, offset)).unwrap()
            } else {
                file.read_at(&mut buf, offset).unwrap()
            };
            assert_eq!(n, count);
            assert!(buf[..n].iter().all(|&byte| byte == 0));
            assert!(buf[n..].iter().all(|&byte| byte == 0xa5));
        }
    }
    assert_eq!(source.reads.load(Relaxed), reads);
}

#[test]
fn symlink_reads_preserve_bytes_and_validate_targets() {
    let mut long = vec![b'x'; 700];
    long[..10].copy_from_slice(b"missing/\xff/");
    let mut nul = long.clone();
    nul[699] = 0;
    for target in [Vec::new(), b"../missing/\xff".to_vec(), long, nul] {
        for inline in [false, true] {
            let mut data = image();
            data[2112] = if inline { 4 } else { 0 };
            (&mut data[2116..]).put_u16_le(0o120777);
            (&mut data[2120..]).put_u32_le(target.len() as u32);
            if inline {
                let head = target.len().saturating_sub(1) / 512 * 512;
                data[5120..5120 + head].copy_from_slice(&target[..head]);
                data[2144..2144 + target.len() - head].copy_from_slice(&target[head..]);
            } else {
                data[5120..5120 + target.len()].copy_from_slice(&target);
            }
            let source = Source {
                data: SliceImage::new(&data),
                reads: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
            };
            let fs = crate::EroFS::new(&source).unwrap();
            let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
            let inode = fs.get_inode(2).unwrap();
            let ainode = ready(afs.get_inode(2)).unwrap();
            let directory = fs.get_inode(1).unwrap();
            source.fail.store(true, Relaxed);
            let reads = source.reads.load(Relaxed);
            assert!(matches!(
                fs.read_link_inode(directory),
                Err(Error::NotASymlink(1))
            ));
            assert!(matches!(
                ready(afs.read_link_inode(directory)),
                Err(Error::NotASymlink(1))
            ));
            assert_eq!(source.reads.load(Relaxed), reads);
            assert!(fs.read_link_inode(inode).is_err());
            assert!(ready(afs.read_link_inode(ainode)).is_err());
            source.fail.store(false, Relaxed);
            let left = fs.read_link_inode(inode);
            let right = ready(afs.read_link_inode(ainode));
            if target.is_empty() || target.contains(&0) {
                assert!(matches!(left, Err(Error::CorruptedData(_))));
                assert!(matches!(right, Err(Error::CorruptedData(_))));
            } else {
                assert_eq!(left.unwrap().as_bytes(), target);
                assert_eq!(right.unwrap().as_bytes(), target);
            }
        }
    }
}
