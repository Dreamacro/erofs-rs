use super::*;
use bytes::Buf;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use crate::{
    backend::SliceImage,
    tests::{Source, check_read_at, ready},
    types::MAGIC_NUMBER,
};
use core::sync::atomic::{AtomicBool, AtomicUsize};

const COMPRESSIONS: [Compression; 5] = [
    Compression::None,
    Compression::Lz4,
    Compression::Lzma,
    Compression::Deflate,
    Compression::Zstd,
];

macro_rules! assert_eq_all {
    ($($actual:expr => $expected:expr);+ $(;)?) => {
        $(assert_eq!($actual, $expected);)+
    };
}

fn extended_metadata() -> Metadata {
    Metadata {
        mode: 0o6751,
        uid: 65536,
        gid: 70000,
        modified: (-2, 123456789),
        ..Metadata::default()
    }
}

fn xattrs<const N: usize>(pairs: [(&[u8], &[u8]); N]) -> Xattrs {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect()
}

fn with_xattrs(xattrs: Xattrs) -> Metadata {
    Metadata {
        xattrs,
        ..Metadata::default()
    }
}

fn source(data: &[u8]) -> Source<'_> {
    Source {
        data: SliceImage::new(data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    }
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    bytes
}

fn collect_entries(
    entries: impl Iterator<Item = Result<crate::WalkDirEntry>>,
) -> Vec<crate::WalkDirEntry> {
    entries.collect::<Result<_>>().unwrap()
}

fn find_inode(entries: &[crate::WalkDirEntry], path: impl AsRef<UnixPath>) -> crate::types::Inode {
    entries
        .iter()
        .find(|entry| entry.dir_entry.path() == path.as_ref())
        .unwrap()
        .inode
}

fn superblock_checksum(image: &[u8]) -> u32 {
    let mut covered = image[1024..4096].to_vec();
    covered[4..8].fill(0);
    !crc32c::crc32c(&covered)
}

#[test]
fn builder_streams_explicit_metadata_and_byte_paths() {
    let metadata = &extended_metadata();
    let mut output = Cursor::new(Vec::new());
    let mut builder = Builder::new(&mut output).unwrap();
    let mut reader = Cursor::new(b"helloextra");
    builder
        .append_file("etc/message", metadata, 5, &mut reader)
        .unwrap();
    assert_eq!(reader.position(), 5); // The suffix belongs to the caller.
    builder
        .append_symlink(
            "link",
            Metadata {
                mode: 0o777,
                ..Metadata::default()
            },
            b"../\xff".as_slice(),
        )
        .unwrap();
    builder.append_hard_link("alias", "/etc/message").unwrap();
    builder.append_hard_link("link-alias", "link").unwrap();
    // A parent can be added after its child; no host filesystem is consulted.
    builder
        .append_dir(
            "etc/",
            Metadata {
                mode: 0o750,
                ..Metadata::default()
            },
        )
        .unwrap();
    builder
        .append_file(
            b"/\xff".as_slice(),
            Metadata::default(),
            65553,
            io::repeat(b'A'),
        )
        .unwrap();
    builder
        .append_dir(
            "/",
            Metadata {
                mode: 0o1755,
                ..metadata.clone()
            },
        )
        .unwrap();
    builder.finish().unwrap();
    assert_eq!(output.position(), output.get_ref().len() as u64);
    let data = output.into_inner();
    assert_eq!(&data[4160..4165], b"hello"); // Inline bytes immediately follow their inode.
    assert_eq!(&data[1024..1028], &MAGIC_NUMBER.to_le_bytes());
    assert_eq!(&data[1032..1036], &1u32.to_le_bytes());
    assert_eq!((&data[1028..]).get_u32_le(), superblock_checksum(&data));
    let base = (&data[1064..]).get_u32_le() as usize * 4096;
    assert_eq!(base, 0);
    assert_eq_all! {
        &data[encode::ROOT_OFFSET..encode::ROOT_OFFSET + 4] => &[5, 0, 0, 0];
        &data[4096 + 4..4096 + 6] => &0o106751u16.to_le_bytes();
        &data[4096 + 24..4096 + 28] => &65536u32.to_le_bytes();
        &data[4096 + 32..4096 + 40] => &(-2i64).to_le_bytes();
        data.len() as u64 => u64::from((&data[1060..]).get_u32_le()) * 4096;
    }
    let source = source(&data);
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let root = fs.get_inode((encode::ROOT_OFFSET / 32) as u64).unwrap();
    assert_eq!(root.nlink(), 3);
    assert_eq!(root.uid(), metadata.uid);
    assert_eq!(root.modified_unix(), metadata.modified);
    for path in [b"/alias".as_slice(), b"/etc/message".as_slice()] {
        let mut file = fs.open(path).unwrap();
        let afile = ready(afs.open(path)).unwrap();
        check_read_at(&file, &afile, b"hello");
        assert_eq!(read_all(&mut file), b"hello");
    }
    let entries = collect_entries(fs.walk_dir("/").unwrap());
    let inode = |path: &[u8]| find_inode(&entries, path);
    let file = inode(b"/alias");
    assert_eq!(file.id(), inode(b"/etc/message").id());
    assert_eq!(file.nlink(), 2);
    assert_eq!(file.gid(), metadata.gid);
    assert_eq!(file.modified_unix(), metadata.modified);
    let link = inode(b"/link");
    assert_eq_all! {
        link.id() => inode(b"/link-alias").id();
        link.nlink() => 2;
        fs.read_link_inode(link).unwrap().as_bytes() => b"../\xff";
        ready(afs.read_link_inode(link)).unwrap().as_bytes() => b"../\xff";
        read_all(fs.open(b"/\xff".as_slice()).unwrap()) => vec![b'A'; 65553];
    }
}

struct Unread;

impl Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("source must not be read")
    }
}

struct Output {
    data: Cursor<Vec<u8>>,
    remaining: usize,
    fail_seek: bool,
    fail_flush: bool,
}

impl Output {
    fn new(remaining: usize) -> Self {
        Self {
            data: Cursor::new(Vec::new()),
            remaining,
            fail_seek: false,
            fail_flush: false,
        }
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(13).min(self.remaining);
        let n = self.data.write(&buf[..n])?;
        self.remaining -= n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            Err(io::Error::other("injected flush failure"))
        } else {
            Ok(())
        }
    }
}

impl Seek for Output {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.fail_seek && position == SeekFrom::Start(1024) {
            Err(io::Error::other("injected seek failure"))
        } else {
            self.data.seek(position)
        }
    }
}

#[test]
fn io_errors_poison_builder_and_drop_never_finishes() {
    fn archive(output: &mut (impl Write + Seek)) -> Result<()> {
        let mut builder = Builder::new(output)?;
        builder.append_file("file", Metadata::default(), 4, &b"data"[..])?;
        builder.finish()?;
        Ok(())
    }
    let mut expected = Cursor::new(Vec::new());
    archive(&mut expected).unwrap();
    let length = expected.get_ref().len();
    for (remaining, fail_seek, fail_flush) in [
        (usize::MAX, false, false),
        (0, false, false),
        (4097, false, false),
        (length - 1, false, false),
        (length + 4, false, false),
        (usize::MAX, true, false),
        (usize::MAX, false, true),
    ] {
        let mut output = Output {
            fail_seek,
            fail_flush,
            ..Output::new(remaining)
        };
        let result = archive(&mut output);
        if remaining == usize::MAX && !fail_seek && !fail_flush {
            result.unwrap();
            assert_eq!(output.data.into_inner(), *expected.get_ref());
        } else {
            assert!(matches!(result, Err(Error::Io(_))));
        }
    }
    for size in [18, (1 << 32) + 17] {
        let mut output = Cursor::new(Vec::new());
        let mut builder = Builder::new(&mut output).unwrap();
        assert!(
            matches!(builder.append_file("short", Metadata::default(), size, &[0; 17][..]), Err(Error::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert!(builder.append_dir("later", Metadata::default()).is_err());
        assert!(builder.finish().is_err());
        assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
    }
    let mut output = Output::new(4097);
    let mut builder = Builder::new(&mut output).unwrap();
    assert!(
        builder
            .append_file("partial", Metadata::default(), 4096, &[0; 4096][..])
            .is_err()
    );
    assert!(
        builder
            .append_file("later", Metadata::default(), 0, Unread)
            .is_err()
    );
    assert!(builder.finish().is_err());
    let mut output = Output::new(4097);
    let mut builder = Builder::new(&mut output).unwrap();
    builder
        .append_file("full-metadata", Metadata::default(), 4032, &[0; 4032][..])
        .unwrap();
    // Flushing the previous metadata block fails before the next source is read.
    assert!(
        builder
            .append_file("next", Metadata::default(), 1, Unread)
            .is_err()
    );
    assert!(builder.append_dir("later", Metadata::default()).is_err());
    assert!(builder.finish().is_err());
    let mut output = Cursor::new(Vec::new());
    {
        let mut builder = Builder::new(&mut output).unwrap();
        builder
            .append_file("file", Metadata::default(), 4, &b"data"[..])
            .unwrap();
    }
    assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
}

#[test]
fn inline_tails_pack_metadata_and_preserve_boundaries() {
    use crate::types::Layout;

    let metadata = &Metadata::default();
    let mut builder = Builder::new(Cursor::new(Vec::new())).unwrap();
    // Regular-file boundaries are covered by the inode-format/xattr matrix below.
    builder.append_file("file-0", metadata, 0, Unread).unwrap();
    for size in [4064, 4065] {
        builder
            .append_symlink(
                format!("link-{size}"),
                metadata,
                vec![b'x'; size].as_slice(),
            )
            .unwrap();
    }
    for dir in ["exact", "plain", "multi"] {
        builder.append_dir(dir, metadata).unwrap();
        // 15 * (12 + 255) + dot entries = 4032 bytes. Compact directory
        // headers also allow the next 13-byte entry to remain inline.
        for n in 0..15 {
            builder
                .append_hard_link(format!("{dir}/{n:03}{}", "x".repeat(252)), "file-0")
                .unwrap();
        }
    }
    builder.append_hard_link("plain/z", "file-0").unwrap();
    builder
        .append_hard_link(format!("multi/{}", "z".repeat(64)), "file-0")
        .unwrap();
    for n in 0..70 {
        builder.append_dir(format!("empty-{n}"), metadata).unwrap();
    }
    let image = builder.finish().unwrap().into_inner();
    let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
    let entries = collect_entries(fs.walk_dir("/").unwrap());
    for (path, size, layout) in [
        (b"/exact".as_slice(), 4032, Layout::FlatInline),
        (b"/plain", 4045, Layout::FlatInline),
        (b"/multi", 4172, Layout::FlatInline),
    ] {
        let inode = find_inode(&entries, path);
        assert_eq!(inode.data_size(), size);
        assert_eq!(inode.layout(), Some(layout));
    }
    for entry in &entries {
        if entry.inode.is_symlink() {
            let size = entry.inode.data_size() as usize;
            assert_eq_all! {
                fs.read_link_inode(entry.inode).unwrap().as_bytes() => vec![b'x'; size];
                entry.inode.layout() => Some(if size == 4064 {
                    Layout::FlatInline
                } else {
                    Layout::FlatPlain
                });
            }
        }
        if entry.inode.layout() == Some(Layout::FlatInline) {
            let tail = (entry.inode.data_size() - 1) % 4096 + 1;
            let at = entry.inode.id() as usize * 32;
            let header = 32 << (image[at] & 1);
            assert!((at + header) % 4096 + tail as usize <= 4096);
        }
    }

    let mut builder = Builder::new(Cursor::new(Vec::new())).unwrap();
    for n in 0..120 {
        builder
            .append_file(format!("tiny-{n:03}"), metadata, 256, &[42; 256][..])
            .unwrap();
        if n == 0 {
            builder.append_hard_link("early", "tiny-000").unwrap();
        }
    }
    builder.append_hard_link("late", "tiny-000").unwrap();
    let image = builder.finish().unwrap().into_inner();
    // Fourteen 288-byte records per block, plus the superblock/root block.
    assert_eq!(image.len(), 10 * 4096);
    let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
    let entries = collect_entries(fs.walk_dir("/").unwrap());
    for entry in entries {
        assert_eq_all!(read_all(fs.open_inode_file(entry.inode).unwrap()) => [42; 256]);
        if matches!(
            entry.dir_entry.file_name(),
            b"early" | b"late" | b"tiny-000"
        ) {
            assert_eq!(entry.inode.nlink(), 3);
        }
    }
    let empty = Builder::new(Cursor::new(Vec::new()))
        .unwrap()
        .finish()
        .unwrap()
        .into_inner();
    assert_eq!(empty.len(), 4096);
    assert!(
        crate::EroFS::new(SliceImage::new(&empty))
            .unwrap()
            .read_dir("/")
            .unwrap()
            .next()
            .is_none()
    );
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "hurd"
))]
mod directory {
    use super::super::directory::{from_directory, scan, write_image};
    use super::*;
    use crate::types::Layout;
    use core::sync::atomic::Ordering::Relaxed;
    use std::{
        ffi::OsStr,
        fs::{self, File, FileTimes, Permissions},
        os::unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt, symlink},
            net::UnixListener,
        },
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    fn set_xattr(path: &std::path::Path, name: &[u8], value: &[u8]) {
        rustix::fs::setxattr(path, name, value, rustix::fs::XattrFlags::empty()).unwrap();
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "erofs-build-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{error}"),
                }
            }
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn images_preserve_bytes_metadata_and_link_identity() {
        let temp = Temp::new();
        let root = temp.0.join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        fs::create_dir(root.join("empty-dir")).unwrap();
        for size in [0, 1, 4095, 4096, 4097, 65553] {
            let data: Vec<_> = (0..size).map(|n| (n % 251) as u8).collect();
            fs::write(root.join(format!("file-{size}")), data).unwrap();
        }
        let raw_name: &[u8] = if cfg!(target_vendor = "apple") {
            b"!name"
        } else {
            b"!\xff"
        };
        fs::write(root.join(OsStr::from_bytes(raw_name)), b"raw name").unwrap();
        for n in 0..80 {
            fs::write(
                root.join("dir").join(format!("{n:03}{}", "x".repeat(252))),
                [],
            )
            .unwrap();
        }
        let file = root.join("file-65553");
        fs::set_permissions(&file, Permissions::from_mode(0o6751)).unwrap();
        File::open(&file)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH - Duration::new(2, 123_456_789)),
            )
            .unwrap();
        fs::hard_link(&file, root.join("dir/alias")).unwrap();
        fs::hard_link(&file, temp.0.join("outside-link")).unwrap();
        symlink(OsStr::from_bytes(b"../missing/\xff"), root.join("dir/link")).unwrap();
        rustix::fs::linkat(
            rustix::fs::CWD,
            root.join("dir/link"),
            rustix::fs::CWD,
            root.join("link-alias"),
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
        let options = Options::default()
            .uuid(core::array::from_fn(|n| n as u8 * 0x11))
            .build_time((-2, 123_456_789));
        let destination = temp.0.join("image.erofs");
        options.build_from_directory(&root, &destination).unwrap();
        let image = fs::read(&destination).unwrap();
        let again = temp.0.join("again.erofs");
        options.build_from_directory(&root, &again).unwrap();
        assert_eq!(image, fs::read(again).unwrap());
        let base = (&image[1064..]).get_u32_le() as usize * 4096;
        let source = source(&image);
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        assert_eq!(fs.super_block().uuid, options.uuid);
        assert_eq!(fs.super_block().created_unix(), Some(options.build_time));
        assert_eq!(fs.super_block().checksum, superblock_checksum(&image));
        assert_eq_all!(fs.get_inode((encode::ROOT_OFFSET / 32) as u64).unwrap().nlink() => 4);
        let entries = collect_entries(fs.walk_dir("/").unwrap());
        assert_eq!(entries.first().unwrap().dir_entry.file_name(), raw_name);
        let mut aentries = ready(afs.walk_dir("/")).unwrap();
        for entry in &entries {
            let aentry = ready(aentries.next_entry()).unwrap().unwrap();
            assert_eq!(entry.dir_entry.path(), aentry.dir_entry.path());
            assert_eq!(entry.inode.id(), aentry.inode.id());
            let path = entry.dir_entry.path();
            let disk = root.join(OsStr::from_bytes(&path.as_bytes()[1..]));
            let metadata = fs::symlink_metadata(&disk).unwrap();
            let inode = entry.inode;
            assert!(matches!(
                inode.layout(),
                Some(Layout::FlatPlain | Layout::FlatInline)
            ));
            assert_eq_all! {
                inode.uid() => metadata.uid();
                inode.gid() => metadata.gid();
                inode.permissions().mode() => metadata.mode() & 0o7777;
                inode.modified_unix() => (metadata.mtime(), metadata.mtime_nsec() as u32);
            }
            let at = base + inode.id() as usize * 32;
            assert_eq_all! {
                &image[at..at + 4] => &[1 | ((inode.layout().unwrap() as u8) << 1), 0, 0, 0];
                &image[at + 8..at + 16] => &inode.data_size().to_le_bytes();
            }
            if inode.is_file() {
                let expected = fs::read(&disk).unwrap();
                let mut file = fs.open_inode_file(inode).unwrap();
                // Path lookup must also search past the first directory block.
                let mut afile = ready(afs.open(&path)).unwrap();
                check_read_at(&file, &afile, &expected);
                assert_eq!(read_all(&mut file), expected);
                let mut position = 0;
                let mut buf = [0; 113];
                loop {
                    let n = ready(afile.read(&mut buf)).unwrap();
                    assert_eq!(&buf[..n], &expected[position..position + n]);
                    position += n;
                    if n == 0 {
                        break;
                    }
                }
                assert_eq!(position, expected.len());
            } else if inode.is_symlink() {
                let target = fs::read_link(disk).unwrap();
                assert_eq_all! {
                    fs.read_link_inode(inode).unwrap().as_bytes() => target.as_os_str().as_bytes();
                    ready(afs.read_link_inode(aentry.inode)).unwrap().as_bytes() =>
                        target.as_os_str().as_bytes();
                }
            }
        }
        assert!(ready(aentries.next_entry()).is_none());
        let inode = |path: &[u8]| find_inode(&entries, path);
        let original = inode(b"/file-65553");
        assert_eq!(original.nlink(), 2);
        assert_eq!(original.id(), inode(b"/dir/alias").id());
        assert_eq!(inode(b"/link-alias").nlink(), 2);
        assert_eq!(inode(b"/link-alias").id(), inode(b"/dir/link").id());
        assert!(fs.read_dir("/empty-dir").unwrap().next().is_none());
    }

    #[cfg(not(target_vendor = "apple"))] // rustix has no mkfifoat on Apple targets.
    #[test]
    fn fifo_import_preserves_metadata_and_links_without_reading_contents() {
        let temp = Temp::new();
        let root = temp.0.join("root");
        fs::create_dir(&root).unwrap();
        let fifo = root.join("fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::empty()).unwrap();
        fs::hard_link(&fifo, root.join("alias")).unwrap();
        fs::hard_link(&fifo, temp.0.join("outside-link")).unwrap();
        let metadata = fs::symlink_metadata(&fifo).unwrap();
        let output = temp.0.join("image.erofs");
        Options::default()
            .build_time((metadata.mtime(), metadata.mtime_nsec() as u32))
            .build_from_directory(&root, &output)
            .unwrap();
        let image = fs::read(&output).unwrap();
        let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
        let entries = collect_entries(fs.read_dir("/").unwrap());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].inode.id(), entries[1].inode.id());
        for entry in entries {
            assert_eq!(entry.dir_entry.file_type(), DirentFileType::Fifo);
            let inode = entry.inode;
            assert_eq_all! {
                inode.file_type() => rustix::fs::FileType::Fifo;
                inode.nlink() => 2;
                image[inode.id() as usize * 32] & 1 =>
                    u8::from(metadata.uid() > 65535 || metadata.gid() > 65535);
                inode.data_size() => 0;
                inode.device() => None;
                inode.layout() => None;
                inode.uid() => metadata.uid();
                inode.gid() => metadata.gid();
                inode.permissions().mode() => metadata.mode() & 0o7777;
                inode.modified_unix() => (metadata.mtime(), metadata.mtime_nsec() as u32);
            }
        }
        let sources = scan(&root).unwrap();
        fs::remove_file(&fifo).unwrap();
        fs::write(&fifo, []).unwrap();
        assert!(write_image(&sources, &mut Cursor::new(Vec::new()), Options::default()).is_err());
    }

    #[test]
    fn directory_import_preserves_xattrs_without_following_links() {
        let temp = Temp::new();
        let root = temp.0.join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        let file = root.join("dir/file");
        fs::write(&file, b"contents").unwrap();
        fs::hard_link(&file, root.join("alias")).unwrap();
        symlink("dir/file", root.join("link")).unwrap();
        let raw_name = if cfg!(target_vendor = "apple") {
            b"user.raw".as_slice()
        } else {
            b"user.\xff"
        };
        let attrs = xattrs([
            (raw_name, b"\0\xffvalue"),
            (b"user.empty", b""),
            (b"user.payload", &[42; 513]),
        ]);
        for (name, value) in &attrs {
            set_xattr(&file, name, value);
        }
        for path in [&root, &root.join("dir")] {
            set_xattr(path, b"user.directory", &[17; 3000]);
        }
        let host = fs::symlink_metadata(&file).unwrap();
        let options = Options::default().build_time((host.mtime(), host.mtime_nsec() as u32));
        let destination = temp.0.join("image.erofs");
        options.build_from_directory(&root, &destination).unwrap();
        let image = fs::read(&destination).unwrap();
        let source = source(&image);
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        for path in ["dir/file", "alias"] {
            assert_eq!(fs.xattrs(path).unwrap(), attrs);
            assert_eq!(ready(afs.xattrs(path)).unwrap(), attrs);
            assert_eq!(read_all(fs.open(path).unwrap()), b"contents");
        }
        for path in ["/", "dir"] {
            assert_eq_all!(fs.xattrs(path).unwrap() => xattrs([(b"user.directory", &[17; 3000])]));
        }
        assert!(fs.xattrs("link").unwrap().is_empty());
        assert!(ready(afs.xattrs("link")).unwrap().is_empty());
        assert_ne!(
            fs.super_block().root_inode_id(),
            (encode::ROOT_OFFSET / 32) as u64
        );
        let sources = scan(&root).unwrap();
        set_xattr(&file, b"user.empty", b"changed");
        assert!(write_image(&sources, &mut Cursor::new(Vec::new()), Options::default()).is_err());
    }

    #[test]
    fn invalid_options_do_not_access_the_source_or_create_or_remove_output() {
        let temp = Temp::new();
        let source = temp.0.join("missing-source");
        let output = temp.0.join("image.erofs");
        let options = Options::default().build_time((0, 1_000_000_000));
        for existing in [false, true] {
            if existing {
                fs::write(&output, b"keep").unwrap();
            }
            assert!(matches!(options.build_from_directory(&source, &output),
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidInput));
            if existing {
                assert_eq!(fs::read(&output).unwrap(), b"keep");
            } else {
                assert!(!output.exists());
            }
        }
    }

    #[test]
    fn rejects_unsupported_inputs_and_never_overwrites_sources() {
        let temp = Temp::new();
        let root = temp.0.join("root");
        fs::create_dir(&root).unwrap();
        let file = fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(temp.0.join("append.erofs"))
            .unwrap();
        let builder = Builder::new(io::BufWriter::new(file)).unwrap();
        assert!(
            matches!(builder.finish(), Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
        let output = temp.0.join("image.erofs");
        from_directory(&root, &output).unwrap();
        let old = fs::read(&output).unwrap();
        assert!(from_directory(&root, &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), old);
        fs::remove_file(&output).unwrap();
        assert!(from_directory(&root, root.join("image")).is_err());
        assert!(!root.join("image").exists());
        symlink(&root, temp.0.join("dir-alias")).unwrap();
        assert!(from_directory(&root, temp.0.join("dir-alias/image")).is_err());
        let file = root.join("file");
        fs::write(&file, b"keep me").unwrap();
        fs::hard_link(&file, &output).unwrap();
        assert!(from_directory(&root, &output).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep me");
        fs::remove_file(&output).unwrap();
        symlink(&file, &output).unwrap();
        assert!(from_directory(&root, &output).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"keep me");
        fs::remove_file(&output).unwrap();
        assert!(from_directory(&file, &output).is_err());
        set_xattr(&file, b"user.key", b"value");
        from_directory(&root, &output).unwrap();
        let image = fs::read(&output).unwrap();
        let image = crate::EroFS::new(SliceImage::new(&image)).unwrap();
        assert_eq_all!(image.xattrs("file").unwrap().get(b"user.key".as_slice()).unwrap() => b"value");
        fs::remove_file(&output).unwrap();
        let sources = scan(&root).unwrap();
        rustix::fs::removexattr(&file, "user.key").unwrap();
        assert!(write_image(&sources, &mut Cursor::new(Vec::new()), Options::default()).is_err());
        let sources = scan(&root).unwrap();
        fs::write(&file, b"changed length").unwrap();
        assert!(write_image(&sources, &mut Cursor::new(Vec::new()), Options::default()).is_err());
        let _socket = UnixListener::bind(root.join("socket")).unwrap();
        assert!(matches!(
            from_directory(&root, &output),
            Err(Error::NotSupported(_))
        ));
        assert!(!output.exists());
    }
}

mod r#async {
    use super::*;
    use crate::backend::{AsyncRead, AsyncSeek, AsyncWrite};
    use InodeFormat::{Auto, Compact, Extended};
    use core::{
        future::{Future, poll_fn},
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use std::cell::Cell;

    // Run identical arguments through both drivers; `.await` marks methods doing I/O.
    macro_rules! both {
        ($sync:ident, $asynchronous:ident; $($method:ident($($arg:expr),* $(,)?) $(.$await:ident)? => $check:ident);+ $(;)?) => {
            $(
                $sync.$method($($arg),*).$check();
                complete(async { $asynchronous.$method($($arg),*)$(.$await)? }).$check();
            )+
        };
        ($sync:ident, $asynchronous:ident, $method:ident($($arg:expr),* $(,)?) $(.$await:ident)?) => {
            [$sync.$method($($arg),*), complete(async { $asynchronous.$method($($arg),*)$(.$await)? })]
        };
    }

    // In-memory I/O wakes immediately but yields between short operations. Requiring
    // Send here also checks every tested public future with Send-but-not-Sync I/O.
    fn complete<T>(future: impl Future<Output = T> + Send) -> T {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        for _ in 0..1_000_000 {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
        panic!("in-memory async I/O made no progress")
    }

    // Poll the same number of times as the scenario requires, then drop to cancel.
    fn cancel_pending(future: impl Future, polls: usize) {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..polls {
            assert!(future.as_mut().poll(&mut cx).is_pending());
        }
    }

    fn finish(builder: AsyncBuilder<AsyncOutput>) -> Vec<u8> {
        complete(builder.finish()).unwrap().inner.data.into_inner()
    }

    struct AsyncOutput {
        inner: Output,
        ready: Cell<bool>,
        freeze_at: Option<u64>,
        pause_superblock: bool,
        fail_end: bool,
    }

    impl AsyncOutput {
        fn new() -> Self {
            Self {
                inner: Output::new(usize::MAX),
                ready: Cell::new(false),
                freeze_at: None,
                pause_superblock: false,
                fail_end: false,
            }
        }

        fn pause(&mut self, cx: &Context<'_>) -> bool {
            let ready = self.ready.get_mut();
            *ready = !*ready;
            if *ready {
                cx.waker().wake_by_ref();
                true
            } else {
                false
            }
        }
    }

    impl AsyncWrite for AsyncOutput {
        async fn write_all(&mut self, mut bytes: &[u8]) -> Result<()> {
            while !bytes.is_empty() {
                let n = poll_fn(|cx| {
                    if self.pause(cx) {
                        return Poll::Pending;
                    }
                    let bytes = if let Some(end) = self.freeze_at {
                        let left = end.saturating_sub(self.inner.data.position());
                        if left == 0 {
                            return Poll::Pending;
                        }
                        &bytes[..bytes.len().min(left as usize)]
                    } else {
                        bytes
                    };
                    Poll::Ready(self.inner.write(bytes))
                })
                .await?;
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero).into());
                }
                bytes = &bytes[n..];
            }
            Ok(())
        }

        async fn flush(&mut self) -> Result<()> {
            poll_fn(|cx| {
                if self.pause(cx) {
                    return Poll::Pending;
                }
                Poll::Ready(self.inner.flush())
            })
            .await?;
            Ok(())
        }
    }

    impl AsyncSeek for AsyncOutput {
        async fn seek(&mut self, position: SeekFrom) -> Result<u64> {
            Ok(poll_fn(|cx| {
                if self.pause(cx) {
                    return Poll::Pending;
                }
                if self.pause_superblock && position == SeekFrom::Start(1024) {
                    return Poll::Pending;
                }
                if self.fail_end
                    && position == SeekFrom::End(0)
                    && self.inner.data.get_ref().len() >= 4096
                {
                    return Poll::Ready(Err(io::Error::other("injected end seek failure")));
                }
                Poll::Ready(self.inner.seek(position))
            })
            .await?)
        }
    }

    struct Reader {
        data: Cursor<Vec<u8>>,
        ready: Cell<bool>,
        fail: bool,
    }

    fn append_stream(
        sync: &mut Builder<Cursor<Vec<u8>>>,
        asynchronous: &mut AsyncBuilder<AsyncOutput>,
        path: impl AsRef<UnixPath> + Sync,
        metadata: &Metadata,
        data: &[u8],
    ) {
        let size = data.len() as u64;
        let mut input = Reader {
            data: Cursor::new([data, b"suffix"].concat()),
            ready: Cell::new(false),
            fail: false,
        };
        sync.append_file(&path, metadata, size, &mut input.data)
            .unwrap();
        assert_eq!(input.data.position(), size);
        input.data.set_position(0);
        complete(asynchronous.append_file(&path, metadata, size, &mut input)).unwrap();
        assert_eq!(input.data.position(), size);
    }

    impl AsyncRead for Reader {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            Ok(poll_fn(|cx| {
                let ready = self.ready.get_mut();
                *ready = !*ready;
                if *ready {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                if self.fail {
                    return Poll::Ready(Err(io::Error::other("injected read failure")));
                }
                let len = 17.min(buf.len());
                Poll::Ready(Read::read(&mut self.data, &mut buf[..len]))
            })
            .await?)
        }
    }

    impl AsyncRead for Unread {
        async fn read(&mut self, _: &mut [u8]) -> Result<usize> {
            panic!("source must not be read")
        }
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    #[test]
    fn compressed_full_indexes_preserve_streams_metadata_and_boundaries() {
        use crate::types::Layout;

        for compression in COMPRESSIONS
            .into_iter()
            .filter(|c| *c != Compression::None && c.enabled())
        {
            for (format, compact, value_len) in [
                (InodeFormat::Auto, true, 0),
                (InodeFormat::Auto, false, 4007),
                (InodeFormat::Auto, true, 4039),
                (InodeFormat::Auto, false, 65535),
                (InodeFormat::Compact, true, 4039),
                (InodeFormat::Extended, false, 0),
            ] {
                let options = Options::default()
                    .compression(compression)
                    .inode_format(format);
                let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
                let mut asynchronous = complete(options.build_async(AsyncOutput::new())).unwrap();
                let mut expected = BTreeMap::new();
                let mut random = 7u32;
                for (i, mut size) in [
                    0, 1, 4095, 4096, 4097, 8191, 8192, 12289, 65535, 65536, 65537,
                ]
                .into_iter()
                .chain([3 * 1024 * 1024 + 17])
                .enumerate()
                {
                    // One large mixed input per codec covers index-page streaming;
                    // the other cases exercise header/xattr boundaries without
                    // repeating the expensive incompressible encoder work.
                    if i == 11 && (format != InodeFormat::Auto || value_len != 0) {
                        size = 65553;
                    }
                    let path = format!("f-{i}");
                    let metadata = Metadata {
                        uid: if !compact && format == InodeFormat::Auto {
                            65536
                        } else {
                            65535
                        },
                        xattrs: if value_len == 0 {
                            Xattrs::new()
                        } else {
                            [(b"user.x".to_vec(), vec![42; value_len])].into()
                        },
                        ..Metadata::default()
                    };
                    let data: Vec<_> = (0..size)
                        .map(|n| {
                            random ^= random << 13;
                            random ^= random >> 17;
                            random ^= random << 5;
                            if i % 3 == 0 || (i % 3 == 2 && n / 65536 % 2 == 0) {
                                0
                            } else {
                                random as u8
                            }
                        })
                        .collect();
                    append_stream(&mut sync, &mut asynchronous, &path, &metadata, &data);
                    let alias = format!("alias-{i}");
                    both!(sync, asynchronous; append_hard_link(&alias, &path) => unwrap);
                    expected.insert(path, (data, metadata));
                }
                // These payload types must never be fed to the compressor.
                let target = vec![b'x'; 8192];
                both!(sync, asynchronous;
                    append_symlink("link", Metadata::default(), target.as_slice()).await => unwrap;
                    append_special("fifo", Metadata::default(), SpecialFile::Fifo).await => unwrap;
                    append_file("empty", Metadata::default(), 0, Unread).await => unwrap;
                );
                let image = sync.finish().unwrap().into_inner();
                assert_eq!(image, finish(asynchronous));
                let source = source(&image);
                let fs = crate::EroFS::new(&source).unwrap();
                let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
                assert_eq_all! {
                    fs.super_block().feature_incompat => if compression == Compression::Lz4 { 1 } else { 3 };
                    fs.super_block().checksum => superblock_checksum(&image);
                    (&image[1108..]).get_u16_le() => if compression == Compression::Lz4 {
                        65535
                    } else {
                        1 << compression.algorithm().unwrap()
                    };
                }
                let entries = collect_entries(fs.read_dir("/").unwrap());
                for (path, (data, metadata)) in &expected {
                    let inode = find_inode(&entries, format!("/{path}"));
                    assert_eq!(inode.nlink(), 2);
                    let offset = inode.id() as usize * 32;
                    assert_eq!(image[offset] & 1, u8::from(!compact));
                    if data.len() >= 8192 {
                        let blocks = (&image[offset + 16..]).get_u32_le() as usize;
                        assert!(blocks > 0 && blocks <= data.len().div_ceil(4096));
                        if data.len() >= 8192 && data.iter().all(|&b| b == 0) {
                            assert!(blocks < data.len().div_ceil(4096));
                        }
                    }
                    assert_eq_all! {
                        inode.uid() => metadata.uid;
                        inode.modified_unix() => metadata.modified;
                        fs.xattrs(path).unwrap() => metadata.xattrs;
                        ready(afs.xattrs(path)).unwrap() => metadata.xattrs;
                        inode.layout() == Some(Layout::CompressedFull) => data.len() >= 8192;
                    }
                    let mut file = fs.open(path).unwrap();
                    assert_eq!(&read_all(&mut file), data);
                    let afile = ready(afs.open(path)).unwrap();
                    check_read_at(&file, &afile, data);
                    for offset in [4095, 4096, 8191, 65535, 65536, 2 * 1024 * 1024 - 1] {
                        let mut buf = [0xa5; 8193];
                        let n = file.read_at(&mut buf, offset).unwrap();
                        let at = (offset as usize).min(data.len());
                        assert_eq!(&buf[..n], &data[at..at + n]);
                        let m = ready(afile.read_at(&mut buf, offset)).unwrap();
                        assert_eq!(n, m);
                        assert_eq!(&buf[..m], &data[at..at + m]);
                    }
                }
                let link = find_inode(&entries, "/link");
                assert_ne!(link.layout(), Some(Layout::CompressedFull));
                assert_eq!(fs.read_link_inode(link).unwrap().as_bytes(), target);
            }
        }
    }

    #[cfg(any(
        feature = "lz4",
        feature = "lzma",
        feature = "deflate",
        feature = "zstd"
    ))]
    #[test]
    fn compression_validation_io_errors_and_cancellation() {
        for compression in COMPRESSIONS
            .into_iter()
            .filter(|c| *c != Compression::None && c.enabled())
        {
            let options = Options::default().compression(compression);
            let metadata = Metadata::default();
            let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
            let mut asynchronous = complete(options.build_async(Cursor::new(Vec::new()))).unwrap();
            for (path, size) in [("../bad", 8192), ("huge", u64::MAX)] {
                both!(sync, asynchronous;
                    append_file(path, &metadata, size, Unread).await => unwrap_err;
                );
            }
            both!(sync, asynchronous;
                append_file("empty", &metadata, 0, Unread).await => unwrap;
            );
            assert_eq!(
                sync.finish().unwrap().into_inner(),
                complete(asynchronous.finish()).unwrap().into_inner()
            );
            for size in [4097, 65536, 65537, (1 << 32) + 17] {
                let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
                let mut asynchronous =
                    complete(options.build_async(Cursor::new(Vec::new()))).unwrap();
                both!(sync, asynchronous;
                    append_file("f", &metadata, size, &b"short"[..]).await => unwrap_err;
                    append_dir("dir", &metadata) => unwrap_err;
                );
            }
            // Data writes, index-page flushes and the final physical-block-count patch.
            for remaining in [
                4096,
                4096 + 13,
                4096 + 32 * 4096 + 13,
                4096 + 48 * 4096 + 4096 + 13,
            ] {
                let output = Output::new(remaining);
                let mut sync = options.build(output).unwrap();
                let mut output = AsyncOutput::new();
                output.inner.remaining = remaining;
                let mut asynchronous = complete(options.build_async(output)).unwrap();
                assert!(
                    sync.append_file("f", &metadata, 3 * 1024 * 1024, io::repeat(0))
                        .is_err()
                );
                let data = vec![0; 3 * 1024 * 1024];
                assert!(
                    complete(asynchronous.append_file(
                        "f",
                        &metadata,
                        data.len() as u64,
                        data.as_slice()
                    ))
                    .is_err()
                );
                assert!(sync.finish().is_err());
                assert!(complete(asynchronous.finish()).is_err());
            }
            let mut output = AsyncOutput::new();
            output.freeze_at = Some(3 * 4096 + 13);
            let mut asynchronous = complete(options.build_async(&mut output)).unwrap();
            let data = vec![0; 3 * 1024 * 1024];
            cancel_pending(
                asynchronous.append_file("f", &metadata, data.len() as u64, data.as_slice()),
                1000,
            );
            assert!(asynchronous.append_dir("dir", &metadata).is_err());
            assert!(complete(asynchronous.finish()).is_err());
            assert_eq!(&output.inner.data.get_ref()[1024..1028], &[0; 4]);
        }
    }

    #[test]
    fn compression_configs_do_not_overlap_root_metadata_and_are_checksummed() {
        for compression in COMPRESSIONS.into_iter().filter(|c| c.enabled()) {
            for (format, value_len, early) in [
                (InodeFormat::Auto, None, true),
                (InodeFormat::Extended, Some(2839), true),
                (InodeFormat::Auto, Some(8192), true),
                (InodeFormat::Auto, Some(8192), false),
            ] {
                let options = Options::default()
                    .compression(compression)
                    .inode_format(format);
                let root = with_xattrs(
                    value_len.map_or_else(Xattrs::new, |len| xattrs([(b"user.a", &vec![42; len])])),
                );
                let mut sync = options.build(Output::new(usize::MAX)).unwrap();
                let mut asynchronous = complete(options.build_async(AsyncOutput::new())).unwrap();
                if early {
                    both!(sync, asynchronous; append_dir("/", &root) => unwrap);
                }
                both!(sync, asynchronous;
                    append_file("file", Metadata::default(), 8209, &[0; 8209][..]).await => unwrap;
                );
                if !early {
                    both!(sync, asynchronous; append_dir("/", &root) => unwrap);
                }
                let image = sync.finish().unwrap().data.into_inner();
                assert_eq!(image, finish(asynchronous));
                let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
                assert_eq!(fs.super_block().checksum, superblock_checksum(&image));
                assert_eq!(fs.xattrs("/").unwrap(), root.xattrs);
                assert_eq!(read_all(fs.open("file").unwrap()), vec![0; 8209]);
                if compression.config_size() != 0 {
                    assert_eq_all! {
                        (&image[1152..]).get_u16_le() as usize => compression.config_size() - 2;
                    }
                    assert!(fs.super_block().root_inode_id() >= 37);
                }
            }
        }
    }

    #[test]
    fn disabled_encoders_do_not_touch_input_or_output() {
        for compression in COMPRESSIONS.into_iter().filter(|c| !c.enabled()) {
            let options = Options::default().compression(compression);
            let mut output = Cursor::new(b"keep".to_vec());
            output.set_position(17);
            assert!(matches!(
                options.build(&mut output),
                Err(Error::NotSupported(_))
            ));
            assert!(matches!(
                complete(options.build_async(&mut output)),
                Err(Error::NotSupported(_))
            ));
            assert_eq!(output.position(), 17);
            assert_eq!(output.into_inner(), b"keep");
            #[cfg(unix)]
            assert!(matches!(
                options.build_from_directory("/missing/source", "/missing/output"),
                Err(Error::NotSupported(_))
            ));
        }
    }

    #[test]
    fn compact_inodes_share_layout_and_preserve_metadata() {
        use crate::types::Layout;
        use SpecialFile::{BlockDevice as Block, CharacterDevice as Char, Fifo};

        for (format, value_len) in [Auto, Compact, Extended].into_iter().flat_map(|format| {
            core::iter::once(None)
                .chain([0, 4043, 4047, 4048, 8192].map(Some))
                .map(move |len| (format, len))
        }) {
            let options = Options::default().inode_format(format);
            let extended = format == InodeFormat::Extended;
            let attrs =
                value_len.map_or_else(Xattrs::new, |len| xattrs([(b"user.a", &vec![0xff; len])]));
            let metadata = with_xattrs(attrs.clone());
            let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
            let mut asynchronous = complete(options.build_async(AsyncOutput::new())).unwrap();
            both!(sync, asynchronous; append_dir("/", &metadata) => unwrap);
            let mut expected = BTreeMap::new();
            for size in [
                0, 1, 31, 32, 4031, 4032, 4033, 4063, 4064, 4065, 4092, 4095, 4096, 4097, 8192,
                12224, 12225,
            ] {
                let name = format!("f-{size}");
                let data: Vec<_> = (0..size).map(|n| (n % 251) as u8).collect();
                append_stream(&mut sync, &mut asynchronous, &name, &metadata, &data);
                expected.insert(format!("/{name}").into_bytes(), data);
            }
            both!(sync, asynchronous;
                append_symlink("symlink", &metadata, "../\u{ff}/raw").await => unwrap;
            );
            for (name, kind) in [
                ("fifo", Fifo),
                (
                    "char",
                    Char {
                        major: 0xfff,
                        minor: 0xfffff,
                    },
                ),
                (
                    "block",
                    Block {
                        major: 8,
                        minor: 257,
                    },
                ),
            ] {
                both!(sync, asynchronous; append_special(name, &metadata, kind).await => unwrap);
            }
            both!(sync, asynchronous; append_dir("dir", &metadata) => unwrap);
            // Patch links only after earlier records have been flushed. Their
            // Header patches must not overwrite xattrs, tails or neighbors.
            for name in expected.keys().map(|p| &p[1..]).chain([
                &b"symlink"[..],
                b"fifo",
                b"char",
                b"block",
            ]) {
                let alias = [b"alias-".as_slice(), name].concat();
                both!(sync, asynchronous; append_hard_link(alias.as_slice(), name) => unwrap);
            }
            let image = sync.finish().unwrap().into_inner();
            assert_eq!(image, finish(asynchronous));
            let source = source(&image);
            let fs = crate::EroFS::new(&source).unwrap();
            let afs = complete(crate::r#async::EroFS::new(&source)).unwrap();
            assert_eq!(fs.super_block().feature_incompat, 0);
            assert_eq!(fs.super_block().checksum, superblock_checksum(&image));
            let root = fs.get_inode(fs.super_block().root_inode_id()).unwrap();
            assert_eq!(image[root.id() as usize * 32] & 1, u8::from(extended));
            assert_eq!(fs.xattrs("/").unwrap(), attrs);
            assert_eq!(root.nlink(), 3);
            for entry in fs.walk_dir("/").unwrap() {
                let entry = entry.unwrap();
                let inode = entry.inode;
                let at = inode.id() as usize * 32;
                assert_eq!(image[at] & 1, u8::from(extended));
                assert_eq!(inode.nlink(), 2);
                assert_eq!(inode.modified_unix(), (0, 0));
                assert_eq!(fs.xattrs_inode(inode).unwrap(), attrs);
                assert_eq!(complete(afs.xattrs_inode(inode)).unwrap(), attrs);
                if inode.layout() == Some(Layout::FlatInline) {
                    let tail = inode.data_size() % 4096;
                    let header = if extended { 64 } else { 32 };
                    assert!((at + header + inode.xattr_size()) % 4096 + tail as usize <= 4096);
                }
                if let Some(data) = expected.get(entry.dir_entry.path().as_bytes()) {
                    if attrs.is_empty() {
                        let tail = data.len() % 4096;
                        let limit = if extended { 4032 } else { 4064 };
                        let layout = if tail != 0 && tail <= limit {
                            Layout::FlatInline
                        } else {
                            Layout::FlatPlain
                        };
                        assert_eq!(inode.layout(), Some(layout), "{:?}", entry.dir_entry.path());
                    }
                    assert_eq!(&read_all(fs.open_inode_file(inode).unwrap()), data);
                    check_read_at(
                        &fs.open_inode_file(inode).unwrap(),
                        &afs.open_inode_file(inode).unwrap(),
                        data,
                    );
                }
            }
        }
        // Check size encoding without streaming multi-GiB inputs.
        let mut node = Node::new(&Metadata::default(), Content::File, u32::MAX.into()).unwrap();
        node.select_format(&Options::default(), 1).unwrap();
        assert!(node.compact);
        assert_eq!((&encode::inode(&node, 0)[8..]).get_u32_le(), u32::MAX);
        node.size += 1;
        node.select_format(&Options::default(), 1).unwrap();
        assert!(!node.compact);
        assert_eq!((&encode::inode(&node, 0)[8..]).get_u64_le(), 1 << 32);
        for time in [(0, 0), (-2, 123), (i64::MIN, 0), (i64::MAX, 999_999_999)] {
            let options = Options::default().build_time(time);
            let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
            let mut asynchronous = complete(options.build_async(Cursor::new(Vec::new()))).unwrap();
            let mut formats = BTreeMap::new();
            let other_second = time.0.checked_add(1).unwrap_or_else(|| time.0 - 1);
            for (name, uid, gid, modified, compact) in [
                ("fits", 65535, 65535, time, true),
                ("uid", 65536, 0, time, false),
                ("gid", 0, 65536, time, false),
                ("default", 0, 0, time, true),
                ("nsec", 0, 0, (time.0, (time.1 + 1) % 1_000_000_000), false),
                ("seconds", 0, 0, (other_second, time.1), false),
            ] {
                let meta = Metadata {
                    uid,
                    gid,
                    modified,
                    ..Metadata::default()
                };
                both!(sync, asynchronous; append_file(name, &meta, 0, Unread).await => unwrap);
                formats.insert(format!("/{name}").into_bytes(), compact);
            }
            let image = sync.finish().unwrap().into_inner();
            assert_eq!(image, complete(asynchronous.finish()).unwrap().into_inner());
            let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
            assert_eq!(fs.super_block().created_unix(), Some(time));
            assert_eq!(fs.super_block().feature_incompat, 0);
            for entry in fs.walk_dir("/").unwrap() {
                let entry = entry.unwrap();
                let path = entry.dir_entry.path();
                let name = path.as_bytes();
                let expected = if name == b"/nsec" {
                    (time.0, (time.1 + 1) % 1_000_000_000)
                } else if name == b"/seconds" {
                    (other_second, time.1)
                } else {
                    time
                };
                assert_eq_all! {
                    entry.inode.modified_unix() => expected;
                    image[entry.inode.id() as usize * 32] & 1 => u8::from(!formats[name]);
                }
            }
        }
    }

    #[test]
    fn inode_formats_preserve_large_link_counts_and_reject_overflow() {
        assert_eq!(InodeFormat::default(), InodeFormat::Auto);
        for (format, count) in [
            (InodeFormat::Auto, 65535u32),
            (InodeFormat::Compact, 65535),
            (InodeFormat::Extended, 65536),
        ] {
            let meta = with_xattrs(xattrs([(b"user.a", b"keep")]));
            let options = Options::default().inode_format(format);
            let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
            let mut asynchronous = complete(options.build_async(Cursor::new(Vec::new()))).unwrap();
            both!(sync, asynchronous;
                append_file("original", &meta, 4, &b"data"[..]).await => unwrap;
                append_file("neighbor", Metadata::default(), 4065, &[42; 4065][..]).await => unwrap;
            );
            for n in 1..count {
                let path = format!("l-{n:05}");
                both!(sync, asynchronous; append_hard_link(&path, "original") => unwrap);
            }
            if format != InodeFormat::Extended {
                for result in both!(sync, asynchronous, append_hard_link("denied", "original")) {
                    assert!(matches!(
                        result,
                        Err(Error::Overflow("compact inode link count"))
                    ));
                }
            }
            both!(sync, asynchronous;
                append_file("denied", Metadata::default(), 0, Unread).await => unwrap;
            );
            let image = sync.finish().unwrap().into_inner();
            assert_eq!(image, complete(asynchronous.finish()).unwrap().into_inner());
            let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
            let entries = collect_entries(fs.walk_dir("/").unwrap());
            let inode = find_inode(&entries, "/original");
            assert_eq!(inode.nlink(), count);
            assert_eq!(image[inode.id() as usize * 32] & 1, u8::from(count > 65535));
            assert_eq!(fs.xattrs("/original").unwrap(), meta.xattrs);
            assert_eq!(read_all(fs.open("/original").unwrap()), b"data");
            assert_eq!(read_all(fs.open("/neighbor").unwrap()), vec![42; 4065]);
        }
        // Directories choose their format after their actual child count is known,
        // including a large root whose metadata space was reserved before payloads.
        for (format, count) in [
            (InodeFormat::Auto, 65535),
            (InodeFormat::Auto, 65536),
            (InodeFormat::Compact, 65536),
            (InodeFormat::Extended, 65535),
        ] {
            let options = Options::default().inode_format(format);
            let mut output = Cursor::new(Vec::new());
            let mut async_output = Cursor::new(Vec::new());
            let mut sync = options.build(&mut output).unwrap();
            let mut asynchronous = complete(options.build_async(&mut async_output)).unwrap();
            let root = with_xattrs(xattrs([(b"user.a", &[7; 4047])]));
            both!(sync, asynchronous; append_dir("/", &root) => unwrap);
            for n in 2..count {
                let path = format!("d-{n:05}");
                both!(sync, asynchronous; append_dir(&path, Metadata::default()) => unwrap);
            }
            if format == InodeFormat::Compact {
                assert!(sync.finish().is_err());
                assert!(complete(asynchronous.finish()).is_err());
                assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
                assert_eq!(&async_output.get_ref()[1024..1028], &[0; 4]);
                continue;
            }
            sync.finish().unwrap();
            complete(asynchronous.finish()).unwrap();
            let image = output.into_inner();
            assert_eq!(image, async_output.into_inner());
            let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
            let inode = fs.get_inode(fs.super_block().root_inode_id()).unwrap();
            assert_eq_all! {
                inode.nlink() => count;
                image[inode.id() as usize * 32] & 1 =>
                    u8::from(count > 65535 || format == InodeFormat::Extended);
                fs.xattrs("/").unwrap() => root.xattrs;
                fs.walk_dir("/").unwrap().count() => count as usize - 2;
                fs.super_block().checksum => superblock_checksum(&image);
            }
        }
    }

    #[test]
    fn forced_compact_validation_is_recoverable_and_does_not_read_input() {
        let options = Options::default().inode_format(InodeFormat::Compact);
        let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
        let mut asynchronous = complete(options.build_async(Cursor::new(Vec::new()))).unwrap();
        for (uid, gid, modified) in [
            (65536, 0, (0, 0)),
            (0, 65536, (0, 0)),
            (0, 0, (1, 0)),
            (0, 0, (0, 1)),
        ] {
            let invalid = Metadata {
                uid,
                gid,
                modified,
                ..Metadata::default()
            };
            both!(sync, asynchronous;
                append_file("f", &invalid, 8192, Unread).await => unwrap_err;
                append_symlink("f", &invalid, "target").await => unwrap_err;
                append_special("f", &invalid, SpecialFile::Fifo).await => unwrap_err;
            );
            for path in ["f", "/"] {
                both!(sync, asynchronous; append_dir(path, &invalid) => unwrap_err);
            }
        }
        both!(sync, asynchronous;
            append_file("f", Metadata::default(), 1 << 32, Unread).await => unwrap_err;
            append_file("f", Metadata::default(), 0, Unread).await => unwrap;
        );
        assert_eq!(
            sync.finish().unwrap().into_inner(),
            complete(asynchronous.finish()).unwrap().into_inner()
        );

        // The implicit root's time is not silently changed to the build time.
        let options = options.build_time((-2, 123));
        assert!(
            options
                .build(Cursor::new(Vec::new()))
                .unwrap()
                .finish()
                .is_err()
        );
        assert!(
            complete(
                complete(options.build_async(Cursor::new(Vec::new())))
                    .unwrap()
                    .finish()
            )
            .is_err()
        );
        let root = Metadata {
            modified: (-2, 123),
            ..Metadata::default()
        };
        let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
        let mut asynchronous = complete(options.build_async(Cursor::new(Vec::new()))).unwrap();
        both!(sync, asynchronous; append_dir("/", &root) => unwrap);
        assert_eq!(
            sync.finish().unwrap().into_inner(),
            complete(asynchronous.finish()).unwrap().into_inner()
        );

        // Import knows the final count before streaming; no public declaration is needed.
        for format in [Auto, Compact, Extended] {
            let mut state = State::new(Options::default().inode_format(format)).unwrap();
            let entry = state.prepare_payload(
                UnixPath::new("f"),
                &Metadata::default(),
                Content::File,
                0,
                65536,
            );
            if format == InodeFormat::Compact {
                assert!(entry.is_err());
                state.check_ready().unwrap();
            } else {
                let entry = entry.unwrap();
                assert!(!entry.node.compact);
                assert_eq!(entry.node.nlink, 1);
            }
        }
    }

    #[test]
    fn inline_xattrs_share_layout_across_blocks_and_entry_types() {
        use SpecialFile::{BlockDevice as Block, CharacterDevice as Char, Fifo};

        let attrs = xattrs([
            (b"user.\xff/name", b"\0\xffvalue"),
            (b"user.empty", b""),
            (b"user.large", &[19; 8192]),
            (&[b"user.".as_slice(), &[b'n'; 250]].concat(), b"long name"),
            (b"trusted.overlay.opaque", b"y"),
            (b"security.selinux", b"opaque\0label"),
            (b"system.posix_acl_access", b"opaque ACL"),
            (b"system.posix_acl_default", b"opaque default ACL"),
        ]);
        let metadata = &with_xattrs(attrs.clone());
        for (root_len, early_root) in [(0, false), (2760, false), (8192, false), (8192, true)] {
            let root = with_xattrs(xattrs([(b"user.root", &vec![7; root_len])]));
            let mut sync = Builder::new(Cursor::new(Vec::new())).unwrap();
            let mut asynchronous = complete(AsyncBuilder::new(AsyncOutput::new())).unwrap();
            if early_root {
                both!(sync, asynchronous; append_dir("/", &root) => unwrap);
            }
            let mut expected_attrs = BTreeMap::new();
            let mut expected_data = BTreeMap::new();
            // A late root with large attributes must also work beyond the legacy NID range.
            if root_len == 8192 {
                let padding = vec![23; 3 * 1024 * 1024];
                both!(sync, asynchronous;
                    append_file("padding", Metadata::default(), padding.len() as u64,
                        padding.as_slice()).await => unwrap;
                );
                expected_attrs.insert(b"/padding".to_vec(), Xattrs::new());
                expected_data.insert(b"/padding".to_vec(), padding);
            }
            // File/xattr size boundaries live in compact_inodes_share_layout_and_preserve_metadata.
            let data: Vec<_> = (0..8193).map(|n| (n % 251) as u8).collect();
            both!(sync, asynchronous;
                append_file("/dir/file", metadata, data.len() as u64, data.as_slice()).await => unwrap;
                append_hard_link("/alias", "/dir/file") => unwrap;
            );
            for path in [b"/dir/file".as_slice(), b"/alias"] {
                expected_attrs.insert(path.to_vec(), attrs.clone());
                expected_data.insert(path.to_vec(), data.clone());
            }
            for (path, kind) in [
                ("/fifo", Fifo),
                ("/char", Char { major: 1, minor: 3 }),
                (
                    "/block",
                    Block {
                        major: 8,
                        minor: 257,
                    },
                ),
            ] {
                both!(sync, asynchronous; append_special(path, metadata, kind).await => unwrap);
                expected_attrs.insert(path.as_bytes().to_vec(), attrs.clone());
            }
            both!(sync, asynchronous;
                append_symlink("/link", metadata, b"../\xff".as_slice()).await => unwrap;
                append_hard_link("/link-alias", "/link") => unwrap;
                append_dir("/dir", metadata) => unwrap;
            );
            for path in ["/link", "/link-alias", "/dir"] {
                expected_attrs.insert(path.as_bytes().to_vec(), attrs.clone());
            }
            if !early_root {
                both!(sync, asynchronous; append_dir("/", &root) => unwrap);
            }
            let image = sync.finish().unwrap().into_inner();
            assert_eq!(image, finish(asynchronous));
            let source = source(&image);
            let fs = crate::EroFS::new(&source).unwrap();
            let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
            assert_eq_all! {
                fs.super_block().checksum => superblock_checksum(&image);
                fs.super_block().feature_compat => 1; // No filter or shared attributes.
                fs.super_block().feature_incompat => if root_len == 8192 && !early_root { 0x80 } else { 0 };
                fs.xattrs("/").unwrap() => root.xattrs;
                ready(afs.xattrs("/")).unwrap() => root.xattrs;
            }
            let entries = collect_entries(fs.walk_dir("/").unwrap());
            assert_eq!(entries.len(), expected_attrs.len());
            for entry in entries {
                let path = entry.dir_entry.path();
                let path = path.as_bytes();
                assert_eq_all! {
                    fs.xattrs_inode(entry.inode).unwrap() => expected_attrs[path];
                    ready(afs.xattrs_inode(entry.inode)).unwrap() => expected_attrs[path];
                }
                if entry.inode.is_file() {
                    check_read_at(
                        &fs.open_inode_file(entry.inode).unwrap(),
                        &afs.open_inode_file(entry.inode).unwrap(),
                        &expected_data[path],
                    );
                } else if entry.inode.is_symlink() {
                    assert_eq_all! {
                        fs.read_link_inode(entry.inode).unwrap().as_bytes() => b"../\xff";
                        ready(afs.read_link_inode(entry.inode)).unwrap().as_bytes() => b"../\xff";
                    }
                }
                if matches!(path, b"/alias" | b"/link-alias") {
                    assert_eq!(entry.inode.nlink(), 2);
                }
            }
        }
    }

    #[test]
    fn inline_xattr_limits_are_checked_before_io() {
        struct SendMetadata {
            metadata: Metadata,
            _not_sync: Cell<()>,
        }

        impl std::borrow::Borrow<Metadata> for SendMetadata {
            fn borrow(&self) -> &Metadata {
                &self.metadata
            }
        }

        let mut attrs = xattrs([
            (b"user.a", &vec![1; 65535]),
            (b"user.b", &vec![2; 65535]),
            (b"user.c", &vec![3; 65535]),
            (b"user.d", &vec![4; 65511]),
        ]);
        let maximum = with_xattrs(attrs.clone());
        attrs
            .get_mut(b"user.d".as_slice())
            .unwrap()
            .extend_from_slice(&[0; 4]);
        let mut invalid = vec![attrs, xattrs([(b"user.big", &vec![0; 65536])])];
        for name in [
            b"".as_slice(),
            b"user.",
            b"user.a\0b",
            b"system.posix_acl_access.extra",
            b"system.unknown",
            b"lustre.foo",
            b"no-namespace",
        ] {
            invalid.push(xattrs([(name, b"")]));
        }
        invalid.push(xattrs([(
            &[b"user.".as_slice(), &[b'n'; 251]].concat(),
            b"",
        )]));
        let mut sync = Builder::new(Cursor::new(Vec::new())).unwrap();
        let mut asynchronous = complete(AsyncBuilder::new(AsyncOutput::new())).unwrap();
        for xattrs in invalid {
            let metadata = with_xattrs(xattrs);
            both!(sync, asynchronous;
                append_dir("/", &metadata) => unwrap_err;
                append_file("bad", &metadata, 1, Unread).await => unwrap_err;
            );
        }
        {
            let _unpolled = asynchronous.append_file("not-added", &maximum, 1, Unread);
        }
        both!(sync, asynchronous;
            append_file("max", &maximum, 4060, &[42; 4060][..]).await => unwrap;
            append_hard_link("alias", "max") => unwrap;
        );
        sync.append_file("neighbor", Metadata::default(), 4, &b"keep"[..])
            .unwrap();
        complete(asynchronous.append_file(
            "neighbor",
            SendMetadata {
                metadata: Metadata::default(),
                _not_sync: Cell::new(()),
            },
            4,
            &b"keep"[..],
        ))
        .unwrap();
        both!(sync, asynchronous; append_dir("/", &maximum) => unwrap);
        let image = sync.finish().unwrap().into_inner();
        assert_eq!(image, finish(asynchronous));
        let source = source(&image);
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        assert_eq!(fs.super_block().checksum, superblock_checksum(&image));
        assert_eq!(fs.xattrs("/").unwrap(), maximum.xattrs);
        assert_eq!(ready(afs.xattrs("/")).unwrap(), maximum.xattrs);
        for entry in fs.walk_dir("/").unwrap() {
            let entry = entry.unwrap();
            let path = entry.dir_entry.path();
            let path = path.as_bytes();
            if path == b"/neighbor" {
                check_read_at(
                    &fs.open_inode_file(entry.inode).unwrap(),
                    &afs.open_inode_file(entry.inode).unwrap(),
                    b"keep",
                );
                assert_eq!(entry.inode.xattr_size(), 0);
            } else {
                assert!(matches!(path, b"/max" | b"/alias"));
                assert_eq!(entry.inode.xattr_size(), 262148);
                assert_eq!(entry.inode.layout(), Some(crate::types::Layout::FlatInline));
                assert_eq!(entry.inode.nlink(), 2);
                let offset = entry.inode.id() as usize * 32;
                assert_eq!(&image[offset + 2..offset + 4], &[255, 255]);
                assert_eq_all! {
                    fs.xattrs_inode(entry.inode).unwrap() => maximum.xattrs;
                    ready(afs.xattrs_inode(entry.inode)).unwrap() => maximum.xattrs;
                }
                check_read_at(
                    &fs.open_inode_file(entry.inode).unwrap(),
                    &afs.open_inode_file(entry.inode).unwrap(),
                    &[42; 4060],
                );
            }
        }
    }

    #[test]
    fn xattr_stream_errors_and_cancellation_require_discard() {
        let metadata = with_xattrs(xattrs([(b"user.large", &[42; 8192])]));
        for stop in [4096 + 93, 8192 + 17] {
            let mut output = Output::new(stop);
            let mut builder = Builder::new(&mut output).unwrap();
            // Attribute streaming fails before touching even an inline payload.
            assert!(builder.append_file("file", &metadata, 1, Unread).is_err());
            assert!(builder.append_dir("later", Metadata::default()).is_err());
            assert!(builder.finish().is_err());
            assert_eq!(output.data.get_ref().len(), stop);
            assert_eq!(&output.data.get_ref()[1024..1152], &[0; 128]);
            for cancel in [false, true] {
                let mut output = AsyncOutput::new();
                if cancel {
                    output.freeze_at = Some(stop as u64);
                } else {
                    output.inner.remaining = stop;
                }
                let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
                if cancel {
                    cancel_pending(builder.append_file("file", &metadata, 1, Unread), 2000);
                } else {
                    assert!(complete(builder.append_file("file", &metadata, 1, Unread)).is_err());
                }
                assert!(builder.append_dir("later", Metadata::default()).is_err());
                assert!(complete(builder.finish()).is_err());
                assert_eq!(output.inner.data.get_ref().len(), stop);
                assert_eq!(&output.inner.data.get_ref()[1024..1152], &[0; 128]);
            }
        }
        let mut output = Output::new(4096 + 93);
        let mut builder = Builder::new(&mut output).unwrap();
        builder.append_dir("/", &metadata).unwrap();
        assert!(builder.finish().is_err());
        assert_eq!(output.data.get_ref().len(), 4096 + 93);
        assert_eq!(&output.data.get_ref()[1024..1152], &[0; 128]);
        let mut output = AsyncOutput::new();
        output.freeze_at = Some(4096 + 93);
        {
            let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
            builder.append_dir("/", &metadata).unwrap();
            cancel_pending(builder.finish(), 2000);
        }
        assert_eq!(output.inner.data.get_ref().len(), 4096 + 93);
        assert_eq!(&output.inner.data.get_ref()[1024..1152], &[0; 128]);
    }

    #[test]
    fn image_options_are_shared_and_do_not_change_inode_times() {
        let metadata = &Metadata {
            modified: (-17, 987_654_321),
            ..Metadata::default()
        };
        let uuid = core::array::from_fn(|n| n as u8 * 0x11);
        let mut previous: Option<Vec<u8>> = None;
        for build_time in [
            (0, 0),
            (-1, 999_999_999),
            (1_700_000_000, 123_456_789),
            (i64::MIN, 0),
            (i64::MAX, 999_999_999),
        ] {
            let options = Options::default().uuid(uuid).build_time(build_time);
            let mut sync = options.build(Cursor::new(Vec::new())).unwrap();
            let mut asynchronous = complete(options.build_async(AsyncOutput::new())).unwrap();
            both!(sync, asynchronous;
                append_file("dir/file", metadata, 4097, &[42; 4097][..]).await => unwrap;
                append_dir("dir", metadata) => unwrap;
            );
            // Keep the root extended: this comparison should change only the superblock.
            let root = Metadata {
                uid: 65536,
                ..Metadata::default()
            };
            both!(sync, asynchronous;
                append_dir("/", &root) => unwrap;
                append_hard_link("alias", "dir/file") => unwrap;
            );
            let image = sync.finish().unwrap().into_inner();
            assert_eq!(image, finish(asynchronous));
            let source = source(&image);
            let fs = crate::EroFS::new(&source).unwrap();
            let afs = complete(crate::r#async::EroFS::new(&source)).unwrap();
            for sb in [fs.super_block(), afs.super_block()] {
                assert_eq!(sb.uuid, uuid);
                assert_eq!(sb.created_unix(), Some(build_time));
                assert_eq!(sb.epoch, build_time.0);
                assert_eq!(sb.fixed_nsec, build_time.1);
                assert_eq!(sb.build_time, 0); // On-disk delta, not absolute creation seconds.
                assert_eq!(sb.checksum, superblock_checksum(&image));
            }
            assert_eq!(&image[1072..1088], &uuid);
            let root = fs.get_inode(fs.super_block().root_inode_id()).unwrap();
            assert_eq!(root.modified_unix(), (0, 0));
            for entry in fs.walk_dir("/").unwrap() {
                assert_eq!(entry.unwrap().inode.modified_unix(), metadata.modified);
            }
            if let Some(previous) = &previous {
                let mut normalized = image.clone();
                for range in [1028..1032, 1048..1060] {
                    normalized[range.clone()].copy_from_slice(&previous[range]);
                }
                assert_eq!(&normalized, previous);
            }
            previous = Some(image);
        }
    }

    #[test]
    fn invalid_build_options_leave_output_untouched() {
        for nanos in [1_000_000_000, u32::MAX] {
            let options = Options::default().build_time((0, nanos));
            for initial in [Vec::new(), b"keep".to_vec()] {
                let mut output = Cursor::new(initial.clone());
                output.set_position(17);
                assert!(matches!(options.build(&mut output),
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidInput));
                assert_eq!(output.position(), 17);
                assert_eq!(output.get_ref(), &initial);
                assert!(matches!(complete(options.build_async(&mut output)),
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidInput));
                assert_eq!(output.position(), 17);
                assert_eq!(output.get_ref(), &initial);
            }
        }
        let mut output = Cursor::new(Vec::new());
        output.set_position(17);
        {
            let _unpolled = Options::default()
                .uuid([0xff; 16])
                .build_time((-1, 1))
                .build_async(&mut output);
        }
        assert_eq!(output.position(), 17);
        assert!(output.get_ref().is_empty());
    }

    #[test]
    fn superblock_crc_covers_final_metadata_and_padding_without_reading_output() {
        for compression in COMPRESSIONS.into_iter().filter(|c| c.enabled()) {
            let options = Options::default().compression(compression);
            for (directories, files, long_names) in [
                (0, 0, false),
                (3, 2, false),
                (80, 1, false),
                (0, 15, true),
                (0, 400, false),
            ] {
                // Neither output implements Read / AsyncRead. Finalization must not
                // reread the superblock's neighboring directory inodes or inline data.
                let mut sync = options.build(Output::new(usize::MAX)).unwrap();
                let mut asynchronous = complete(options.build_async(AsyncOutput::new())).unwrap();
                let metadata = &Metadata::default();
                for n in 0..files {
                    let path = if long_names {
                        format!("{n:03}{}", "x".repeat(252))
                    } else {
                        format!("file-{n:03}")
                    };
                    both!(sync, asynchronous;
                        append_file(&path, metadata, 4, &b"data"[..]).await => unwrap;
                    );
                }
                for n in 0..directories {
                    let path = format!("dir-{n:03}");
                    both!(sync, asynchronous; append_dir(&path, metadata) => unwrap);
                }
                if files != 0 && !long_names {
                    both!(sync, asynchronous; append_hard_link("alias", "file-000") => unwrap);
                }
                let image = sync.finish().unwrap().data.into_inner();
                assert_eq!(image, finish(asynchronous));
                assert_eq!(&image[1032..1036], &1u32.to_le_bytes());
                let checksum = (&image[1028..]).get_u32_le();
                assert_eq!(checksum, superblock_checksum(&image));
                if directories == 0 && files == 0 {
                    assert_eq!(image.len(), 4096);
                }
                if long_names {
                    let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
                    let root = fs.get_inode(fs.super_block().root_inode_id()).unwrap();
                    assert_eq!(root.layout(), Some(crate::types::Layout::FlatPlain));
                }
            }
        }
    }

    #[test]
    fn final_checksummed_header_write_errors_and_cancellation_are_not_success() {
        // An empty image writes the initial block and the final directory block
        // before publishing the header and codec config. Fail within that last write.
        for compression in COMPRESSIONS.into_iter().filter(|c| c.enabled()) {
            let options = Options::default().compression(compression);
            for bytes in [0, 4, 8, 127 + compression.config_size()] {
                let output = Output::new(8192 + bytes);
                assert!(options.build(output).unwrap().finish().is_err());
                let mut output = AsyncOutput::new();
                output.inner.remaining = 8192 + bytes;
                let builder = complete(options.build_async(output)).unwrap();
                assert!(complete(builder.finish()).is_err());
            }
        }
        let mut output = AsyncOutput::new();
        output.pause_superblock = true;
        let builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        cancel_pending(builder.finish(), 1024);
        let image = output.inner.data.get_ref();
        assert_eq!(image.len(), 4096);
        assert_eq!(&image[1024..1152], &[0; 128]); // No early publication of the CRC/header.
        assert_eq!(&image[1152..1154], &[4, 0]); // Compact directory metadata was already written.
    }

    #[test]
    fn special_files_share_encoding_validation_and_hard_links() {
        use crate::types::DirentFileType as Kind;
        use SpecialFile::{BlockDevice as Block, CharacterDevice as Char, Fifo};
        use std::os::unix::fs::PermissionsExt;

        let metadata = &extended_metadata();
        let mut sync = Builder::new(Cursor::new(Vec::new())).unwrap();
        let mut asynchronous = complete(AsyncBuilder::new(AsyncOutput::new())).unwrap();
        for (major, minor) in [(0x1000, 0), (0, 0x100000), (u32::MAX, u32::MAX)] {
            for kind in [Char { major, minor }, Block { major, minor }] {
                for result in both!(
                    sync,
                    asynchronous,
                    append_special("bad", metadata, kind).await
                ) {
                    assert!(matches!(result,
                        Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidInput));
                }
            }
        }
        // The first special inode must flush a full inline-data block.
        both!(sync, asynchronous;
            append_file("head", metadata, 4032, &[42; 4032][..]).await => unwrap;
        );
        #[rustfmt::skip] // Keep each input beside its independent on-disk expectations.
        let cases = [
            (Fifo, Kind::Fifo, 0o010000, None, 0u32),
            (Char { major: 1, minor: 3 }, Kind::CharacterDevice,
                0o020000, Some((1, 3)), 0x103),
            (Block { major: 8, minor: 257 }, Kind::BlockDevice,
                0o060000, Some((8, 257)), 0x100801),
            (Char { major: 0, minor: 0 }, Kind::CharacterDevice,
                0o020000, Some((0, 0)), 0),
            (Char { major: 0xabc, minor: 0x54321 }, Kind::CharacterDevice,
                0o020000, Some((0xabc, 0x54321)), 0x543abc21),
            (Block { major: 0xfff, minor: 0xfffff }, Kind::BlockDevice,
                0o060000, Some((0xfff, 0xfffff)), u32::MAX),
        ];
        let paths: Vec<_> = (0..70)
            .map(|n| {
                if n == 0 {
                    b"dev/\xff".to_vec()
                } else {
                    format!("dev/{n:03}").into_bytes()
                }
            })
            .collect();
        for (n, path) in paths.iter().enumerate() {
            let kind = cases[n % cases.len()].0;
            {
                let _unpolled = asynchronous.append_special(path.as_slice(), metadata, kind);
            }
            both!(sync, asynchronous;
                append_special(path.as_slice(), metadata, kind).await => unwrap;
            );
            if n == 0 {
                // A neighboring inline tail must survive later hard-link header patches.
                both!(sync, asynchronous;
                    append_file("keep", metadata, 4, &b"tail"[..]).await => unwrap;
                );
            }
        }
        for (n, path) in paths.iter().take(cases.len()).enumerate() {
            both!(sync, asynchronous;
                append_hard_link(format!("alias-{n}"), path.as_slice()) => unwrap;
            );
        }
        // Special inodes obey the same namespace rules as regular files.
        both!(sync, asynchronous;
            append_special("dev/001", metadata, SpecialFile::Fifo).await => unwrap_err;
            append_dir("dev/001/child", metadata) => unwrap_err;
            append_special("dev", metadata, SpecialFile::Fifo).await => unwrap_err;
            append_dir("dev", metadata) => unwrap;
        );
        let image = sync.finish().unwrap().into_inner();
        assert_eq!(image, finish(asynchronous));
        assert_eq!(image.len(), 4 * 4096); // Only metadata blocks, no special-file payload.
        let source = source(&image);
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = complete(crate::r#async::EroFS::new(&source)).unwrap();
        let entries = collect_entries(fs.walk_dir("/").unwrap());
        let mut aentries = complete(afs.walk_dir("/")).unwrap();
        for entry in &entries {
            let aentry = complete(aentries.next_entry()).unwrap().unwrap();
            assert_eq!(entry.dir_entry.path(), aentry.dir_entry.path());
            assert_eq!(entry.inode.id(), aentry.inode.id());
            assert_eq!(entry.inode.file_type(), aentry.inode.file_type());
            assert_eq!(entry.inode.device(), aentry.inode.device());
            assert_eq!(entry.inode.nlink(), aentry.inode.nlink());
        }
        assert!(complete(aentries.next_entry()).is_none());
        for (n, path) in paths.iter().enumerate() {
            let entry = entries
                .iter()
                .find(|entry| &entry.dir_entry.path().as_bytes()[1..] == path)
                .unwrap();
            let (_, kind, mode, device, rdev) = cases[n % cases.len()];
            let inode = entry.inode;
            assert_eq_all! {
                entry.dir_entry.file_type() => kind;
                inode.file_type() => rustix::fs::FileType::from_raw_mode(mode as _);
                inode.device() => device;
                inode.layout() => None;
                inode.data_size() => 0;
                inode.uid() => metadata.uid;
                inode.gid() => metadata.gid;
                inode.permissions().mode() => u32::from(metadata.mode);
                inode.modified_unix() => metadata.modified;
                inode.nlink() => if n < cases.len() { 2 } else { 1 };
            }
            let at = inode.id() as usize * 32;
            assert_eq_all! {
                &image[at..at + 4] => &[1, 0, 0, 0];
                &image[at + 4..at + 6] => &(mode | metadata.mode).to_le_bytes();
                &image[at + 6..at + 16] => &[0; 10];
                &image[at + 16..at + 20] => &rdev.to_le_bytes();
            }
            assert!(matches!(fs.open_inode_file(inode), Err(Error::NotAFile(_))));
            assert!(matches!(
                afs.open_inode_file(inode),
                Err(Error::NotAFile(_))
            ));
            if n < cases.len() {
                let alias = format!("/alias-{n}");
                assert_eq!(inode.id(), find_inode(&entries, &alias).id());
            }
        }
        for (path, expected) in [("head", vec![42; 4032]), ("keep", b"tail".to_vec())] {
            assert_eq!(read_all(fs.open(path).unwrap()), expected);
        }
    }

    #[test]
    fn special_append_flush_failures_and_cancellation_poison_builders() {
        let metadata = &Metadata::default();
        let mut output = Output::new(4097);
        let mut sync = Builder::new(&mut output).unwrap();
        sync.append_file("full", metadata, 4064, &[0; 4064][..])
            .unwrap();
        assert!(
            sync.append_special("fifo", metadata, SpecialFile::Fifo)
                .is_err()
        );
        assert!(
            sync.append_special("later", metadata, SpecialFile::Fifo)
                .is_err()
        );
        assert!(sync.finish().is_err());
        assert_eq!(output.data.get_ref().len(), 4097);
        assert_eq!(&output.data.get_ref()[1024..1028], &[0; 4]);
        for cancel in [false, true] {
            let mut output = AsyncOutput::new();
            if cancel {
                output.freeze_at = Some(4097);
            } else {
                output.inner.remaining = 4097;
            }
            let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
            complete(builder.append_file("full", metadata, 4064, &[0; 4064][..])).unwrap();
            if cancel {
                cancel_pending(
                    builder.append_special("fifo", metadata, SpecialFile::Fifo),
                    8,
                );
            } else {
                assert!(
                    complete(builder.append_special("fifo", metadata, SpecialFile::Fifo)).is_err()
                );
            }
            assert!(
                complete(builder.append_special("later", metadata, SpecialFile::Fifo)).is_err()
            );
            assert!(complete(builder.finish()).is_err());
            assert_eq!(output.inner.data.get_ref().len(), 4097);
            assert_eq!(&output.inner.data.get_ref()[1024..1028], &[0; 4]);
        }
    }

    #[test]
    fn async_erofs_files_can_be_streamed_into_builder() {
        let metadata = &Metadata::default();
        let data: Vec<_> = (0..65553).map(|n| (n % 251) as u8).collect();
        let mut builder = Builder::new(Cursor::new(Vec::new())).unwrap();
        builder
            .append_file("original", metadata, data.len() as u64, data.as_slice())
            .unwrap();
        let image = builder.finish().unwrap().into_inner();
        let source = source(&image);
        let fs = complete(crate::r#async::EroFS::new(&source)).unwrap();
        // Use an EROFS file directly, without a Tokio adapter.
        let mut file = complete(fs.open("original")).unwrap();
        let mut rewritten = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
        complete(rewritten.append_file("copy", metadata, file.size(), &mut file)).unwrap();
        assert_eq!(complete(file.read(&mut [0; 1])).unwrap(), 0);
        let image = complete(rewritten.finish()).unwrap().into_inner();
        let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
        assert_eq!(read_all(fs.open("copy").unwrap()), data);
    }

    #[test]
    fn validation_does_not_consume_data_or_poison_either_builder() {
        let metadata = &Metadata::default();
        let mut sync = Builder::new(Cursor::new(Vec::new())).unwrap();
        let mut asynchronous = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
        for path in [
            "", "/", ".", "..", "../bad", "a//b", "//a", "a/./b", "a/../b", "a/", "a\0b",
        ] {
            both!(sync, asynchronous; append_file(path, metadata, 1, Unread).await => unwrap_err);
        }
        both!(sync, asynchronous;
            append_file(vec![b'a'; 256].as_slice(), metadata, 1, Unread).await => unwrap_err;
            append_dir("//", metadata) => unwrap_err;
            append_dir("a//", metadata) => unwrap_err;
        );
        for (mode, modified) in [(0o100644, (0, 0)), (0o644, (0, 1_000_000_000))] {
            let invalid = Metadata {
                mode,
                modified,
                ..Metadata::default()
            };
            both!(sync, asynchronous; append_file("bad", &invalid, 1, Unread).await => unwrap_err);
        }
        for result in both!(
            sync,
            asynchronous,
            append_file("huge", metadata, u64::MAX, Unread).await
        ) {
            assert!(matches!(result, Err(Error::Overflow(_))));
        }
        for target in ["", "x\0y", "a\0b"] {
            both!(sync, asynchronous; append_symlink("bad", metadata, target).await => unwrap_err);
        }
        {
            let _unpolled = asynchronous.append_file("not-added", metadata, 1, Unread);
        }
        both!(sync, asynchronous;
            append_file("empty", metadata, 0, Unread).await => unwrap;
            append_file("/empty", metadata, 1, Unread).await => unwrap_err;
            append_dir("empty/child", metadata) => unwrap_err;
            append_file("empty/child", metadata, 1, Unread).await => unwrap_err;
            append_hard_link("bad", "not-yet-added") => unwrap_err;
            append_file("future/file", metadata, 0, Unread).await => unwrap;
            append_file("future", metadata, 1, Unread).await => unwrap_err;
            append_symlink("future", metadata, "empty").await => unwrap_err;
            append_hard_link("future", "empty") => unwrap_err;
            append_dir("future", metadata) => unwrap;
            append_hard_link("bad", "future") => unwrap_err;
            append_dir("/", metadata) => unwrap;
            append_dir("", metadata) => unwrap_err;
            append_dir("dir", metadata) => unwrap;
            append_hard_link("bad", "dir") => unwrap_err;
            append_hard_link("bad", "unknown") => unwrap_err;
            finish().await => unwrap;
        );
        let mut existing = Cursor::new(b"keep".to_vec());
        assert!(Builder::new(&mut existing).is_err());
        assert_eq!(existing.get_ref(), b"keep");
        assert!(complete(AsyncBuilder::new(&mut existing)).is_err());
        assert_eq!(existing.into_inner(), b"keep");
        let mut output = Cursor::new(Vec::new());
        let mut async_output = Cursor::new(Vec::new());
        let mut sync = Builder::new(&mut output).unwrap();
        let mut asynchronous = complete(AsyncBuilder::new(&mut async_output)).unwrap();
        both!(sync, asynchronous;
            append_file("missing/file", metadata, 0, Unread).await => unwrap;
        );
        // Finish returns different writer types, so compare the errors separately.
        assert!(matches!(sync.finish(), Err(Error::PathNotFound(_))));
        assert!(matches!(
            complete(asynchronous.finish()),
            Err(Error::PathNotFound(_))
        ));
        for output in [output, async_output] {
            assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
        }
    }

    #[test]
    fn async_io_failures_poison_builder() {
        let metadata = &Metadata::default();
        for (size, input_size) in [(18, 17), (4097, 4096), ((1 << 32) + 17, 17)] {
            let input = vec![0; input_size];
            let mut output = Cursor::new(Vec::new());
            let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
            assert!(
                matches!(complete(builder.append_file("short", metadata, size, input.as_slice())), Err(Error::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
            );
            assert!(builder.append_dir("later", metadata).is_err());
            assert!(complete(builder.finish()).is_err());
            assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
        }

        struct Oversized;

        impl AsyncRead for Oversized {
            async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
                Ok(buf.len() + 1)
            }
        }
        for size in [1, 4096] {
            let mut builder = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
            assert!(
                matches!(complete(builder.append_file("bad", metadata, size, Oversized)), Err(Error::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
            );
            assert!(complete(builder.finish()).is_err());
        }

        let mut builder = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
        let source = Reader {
            data: Cursor::new(Vec::new()),
            ready: Cell::new(false),
            fail: true,
        };
        assert!(complete(builder.append_file("bad", metadata, 1, source)).is_err());
        assert!(complete(builder.finish()).is_err());

        async fn archive(output: &mut AsyncOutput) -> Result<()> {
            let mut builder = AsyncBuilder::new(output).await?;
            builder
                .append_file("file", Metadata::default(), 4, &b"data"[..])
                .await?;
            builder.finish().await?;
            Ok(())
        }
        let mut expected = AsyncOutput::new();
        complete(archive(&mut expected)).unwrap();
        let length = expected.inner.data.get_ref().len();
        for remaining in [0, 4097, length - 1, length + 4] {
            let mut output = AsyncOutput::new();
            output.inner.remaining = remaining;
            assert!(complete(archive(&mut output)).is_err());
        }
        for (fail_seek, fail_flush, fail_end) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let mut output = AsyncOutput::new();
            output.inner.fail_seek = fail_seek;
            output.inner.fail_flush = fail_flush;
            output.fail_end = fail_end;
            // No file copy: specifically exercise seek/flush failures in finish().
            let builder = complete(AsyncBuilder::new(output)).unwrap();
            assert!(complete(builder.finish()).is_err());
        }
    }

    #[test]
    fn async_partial_inline_tail_cancellation_poisons_builder() {
        struct PendingTail<'a>(&'a [u8]);

        impl AsyncRead for PendingTail<'_> {
            async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
                if self.0.is_empty() && !buf.is_empty() {
                    core::future::pending().await
                } else {
                    AsyncRead::read(&mut self.0, buf).await
                }
            }
        }

        let metadata = &Metadata::default();
        let mut output = Cursor::new(Vec::new());
        let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        let mut source = PendingTail(&[42; 4097]);
        // The full external block and one inline byte are consumed before
        // the source stalls on the remaining inline byte.
        cancel_pending(
            builder.append_file("cancelled", metadata, 4098, &mut source),
            1,
        );
        assert!(source.0.is_empty());
        assert!(builder.append_dir("later", metadata).is_err());
        assert!(complete(builder.finish()).is_err());
        assert_eq!(&output.get_ref()[8192..], &[42; 4096]);
        assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
    }

    #[test]
    fn async_cancellation_poisons_builder() {
        let metadata = &Metadata::default();
        // Cancel a polled append after a partial payload or during padding.
        for (size, stop) in [(4096, 8193), (4065, 8192 + 4065)] {
            let mut output = AsyncOutput::new();
            output.freeze_at = Some(stop);
            let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
            cancel_pending(
                builder.append_file("cancelled", metadata, size, &[0; 4096][..]),
                1024,
            );
            assert!(builder.append_dir("later", metadata).is_err());
            assert!(complete(builder.finish()).is_err());
            assert_eq!(output.inner.data.get_ref().len(), stop as usize);
            assert_eq!(&output.inner.data.get_ref()[1024..1028], &[0; 4]);
        }
        // Cancellation while flushing a packed metadata block, before reading
        // the next source, must poison the builder as well.
        let mut output = AsyncOutput::new();
        output.freeze_at = Some(4097);
        let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        complete(builder.append_file("full-metadata", metadata, 4032, &[0; 4032][..])).unwrap();
        cancel_pending(builder.append_file("next", metadata, 1, Unread), 8);
        assert!(builder.append_dir("later", metadata).is_err());
        assert!(complete(builder.finish()).is_err());
        assert_eq!(output.inner.data.get_ref().len(), 4097);

        // Cancellation while the source is pending, before any payload is written.
        let mut builder = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
        {
            let source = Reader {
                data: Cursor::new(b"data".to_vec()),
                ready: Cell::new(false),
                fail: false,
            };
            cancel_pending(builder.append_file("cancelled", metadata, 4, source), 1);
        }
        assert!(builder.append_hard_link("later", "cancelled").is_err());
        assert!(complete(builder.finish()).is_err());

        let mut output = AsyncOutput::new();
        output.freeze_at = Some(4097);
        let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        complete(builder.append_file("empty", metadata, 0, Unread)).unwrap();
        cancel_pending(builder.finish(), 8);
        assert_eq!(output.inner.data.get_ref().len(), 4097);
        assert_eq!(&output.inner.data.get_ref()[1024..1028], &[0; 4]);
        let mut output = AsyncOutput::new();
        output.freeze_at = Some(1);
        cancel_pending(AsyncBuilder::new(&mut output), 16);
        assert_eq!(output.inner.data.get_ref(), &[0]);
    }
}
