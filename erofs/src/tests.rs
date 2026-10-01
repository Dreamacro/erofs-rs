use alloc::vec::Vec;
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
        data[at..at + 8].copy_from_slice(&nid.to_le_bytes());
        data[at + 8..at + 10].copy_from_slice(&(name_offset as u16).to_le_bytes());
        data[at + 10] = kind;
        data[name_offset..name_offset + name.len()].copy_from_slice(name);
        name_offset += name.len();
    }
}

pub fn image() -> Vec<u8> {
    let mut data = vec![0; 6656];
    data[1024..1028].copy_from_slice(&MAGIC_NUMBER.to_le_bytes());
    data[1036] = 9;
    data[1038] = 1;
    data[1064] = 4;
    for (nid, mode, size, block) in [
        (1, 0o40755u16, 1024u32, 8u32),
        (2, 0o100644, 700, 10),
        (3, 0o100644, 1, 12),
    ] {
        let at = 2048 + nid * 32;
        data[at + 4..at + 6].copy_from_slice(&mode.to_le_bytes());
        data[at + 8..at + 12].copy_from_slice(&size.to_le_bytes());
        data[at + 16..at + 20].copy_from_slice(&block.to_le_bytes());
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
    data[2116..2118].copy_from_slice(&0o40755u16.to_le_bytes());
    data[2120..2124].copy_from_slice(&512u32.to_le_bytes());
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
