use super::*;
use bytes::Buf;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use crate::{
    backend::SliceImage,
    tests::{Source, check_read_at, ready},
    types::MAGIC_NUMBER,
};
use core::sync::atomic::{AtomicBool, AtomicUsize};

#[test]
fn builder_streams_explicit_metadata_and_byte_paths() {
    let metadata = Metadata {
        mode: 0o6751,
        uid: 65536,
        gid: 70000,
        modified: (-2, 123456789),
    };
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
                ..metadata
            },
        )
        .unwrap();
    builder.finish().unwrap();
    assert_eq!(output.position(), output.get_ref().len() as u64);
    let data = output.into_inner();
    assert_eq!(&data[4160..4165], b"hello"); // Inline bytes immediately follow their inode.
    assert_eq!(&data[1024..1028], &MAGIC_NUMBER.to_le_bytes());
    assert_eq!(&data[1028..1036], &[0; 8]);
    let base = (&data[1064..]).get_u32_le() as usize * 4096;
    assert_eq!(base, 0);
    assert_eq!(
        &data[encode::ROOT_OFFSET..encode::ROOT_OFFSET + 4],
        &[5, 0, 0, 0]
    );
    assert_eq!(&data[4096 + 4..4096 + 6], &0o106751u16.to_le_bytes());
    assert_eq!(&data[4096 + 24..4096 + 28], &65536u32.to_le_bytes());
    assert_eq!(&data[4096 + 32..4096 + 40], &(-2i64).to_le_bytes());
    assert_eq!(
        data.len() as u64,
        u64::from((&data[1060..]).get_u32_le()) * 4096
    );
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
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
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"hello");
    }
    let entries = fs
        .walk_dir("/")
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    let inode = |path: &[u8]| {
        entries
            .iter()
            .find(|entry| entry.dir_entry.path().as_bytes() == path)
            .unwrap()
            .inode
    };
    let file = inode(b"/alias");
    assert_eq!(file.id(), inode(b"/etc/message").id());
    assert_eq!(file.nlink(), 2);
    assert_eq!(file.gid(), metadata.gid);
    assert_eq!(file.modified_unix(), metadata.modified);
    let link = inode(b"/link");
    assert_eq!(link.id(), inode(b"/link-alias").id());
    assert_eq!(link.nlink(), 2);
    assert_eq!(fs.read_link_inode(link).unwrap().as_bytes(), b"../\xff");
    assert_eq!(
        ready(afs.read_link_inode(link)).unwrap().as_bytes(),
        b"../\xff"
    );
    let mut bytes = Vec::new();
    fs.open(b"/\xff".as_slice())
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, vec![b'A'; 65553]);
}

struct Unread;

impl Read for Unread {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("source must not be read")
    }
}

#[test]
fn validation_does_not_consume_data_or_poison_the_builder() {
    let metadata = Metadata::default();
    let mut builder = Builder::new(Cursor::new(Vec::new())).unwrap();
    for path in [
        b"".as_slice(),
        b"/",
        b".",
        b"..",
        b"a//b",
        b"//a",
        b"a/./b",
        b"a/../b",
        b"a/",
        b"a\0b",
    ] {
        assert!(builder.append_file(path, metadata, 1, Unread).is_err());
    }
    assert!(
        builder
            .append_file(vec![b'a'; 256].as_slice(), metadata, 1, Unread)
            .is_err()
    );
    assert!(builder.append_dir("//", metadata).is_err());
    assert!(builder.append_dir("a//", metadata).is_err());
    for invalid in [
        Metadata {
            mode: 0o100644,
            ..metadata
        },
        Metadata {
            modified: (0, 1_000_000_000),
            ..metadata
        },
    ] {
        assert!(builder.append_file("bad", invalid, 1, Unread).is_err());
    }
    assert!(matches!(
        builder.append_file("huge", metadata, u64::MAX, Unread),
        Err(Error::Overflow(_))
    ));
    for target in [b"".as_slice(), b"x\0y"] {
        assert!(builder.append_symlink("bad", metadata, target).is_err());
    }
    builder.append_file("empty", metadata, 0, Unread).unwrap();
    assert!(builder.append_file("/empty", metadata, 1, Unread).is_err());
    assert!(builder.append_dir("empty/child", metadata).is_err());
    assert!(
        builder
            .append_file("empty/child", metadata, 1, Unread)
            .is_err()
    );
    assert!(builder.append_hard_link("bad", "not-yet-added").is_err());
    builder
        .append_file("future/file", metadata, 0, Unread)
        .unwrap();
    assert!(builder.append_file("future", metadata, 1, Unread).is_err());
    assert!(builder.append_symlink("future", metadata, "empty").is_err());
    assert!(builder.append_hard_link("future", "empty").is_err());
    builder.append_dir("future", metadata).unwrap();
    assert!(builder.append_hard_link("bad", "future").is_err());
    builder.append_dir("/", metadata).unwrap();
    assert!(builder.append_dir("", metadata).is_err());
    builder.finish().unwrap();

    let mut missing = Cursor::new(Vec::new());
    let mut builder = Builder::new(&mut missing).unwrap();
    builder
        .append_file("missing/file", metadata, 0, Unread)
        .unwrap();
    assert!(matches!(builder.finish(), Err(Error::PathNotFound(_))));
    assert_eq!(&missing.get_ref()[1024..1028], &[0; 4]);
    let mut existing = Cursor::new(b"keep".to_vec());
    assert!(Builder::new(&mut existing).is_err());
    assert_eq!(existing.into_inner(), b"keep");
}

struct Output {
    data: Cursor<Vec<u8>>,
    remaining: usize,
    fail_seek: bool,
    fail_flush: bool,
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
            data: Cursor::new(Vec::new()),
            remaining,
            fail_seek,
            fail_flush,
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
    let mut output = Output {
        data: Cursor::new(Vec::new()),
        remaining: 4097,
        fail_seek: false,
        fail_flush: false,
    };
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
    let mut output = Output {
        data: Cursor::new(Vec::new()),
        remaining: 4097,
        fail_seek: false,
        fail_flush: false,
    };
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

    let metadata = Metadata::default();
    let mut builder = Builder::new(Cursor::new(Vec::new())).unwrap();
    let sizes = [
        0, 1, 31, 32, 4031, 4032, 4033, 4095, 4096, 4097, 8192, 12224, 12225,
    ];
    for size in sizes {
        let bytes: Vec<_> = (0..size).map(|n| (n % 251) as u8).collect();
        let mut reader = Cursor::new([bytes.as_slice(), b"suffix"].concat());
        builder
            .append_file(format!("file-{size}"), metadata, size as u64, &mut reader)
            .unwrap();
        assert_eq!(reader.position(), size as u64);
    }
    for size in [4032, 4033] {
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
        // 15 * (12 + 255) + dot entries = 4032 bytes, exactly filling a
        // metadata block together with one extended inode.
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
    let entries = fs
        .walk_dir("/")
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    for size in sizes {
        let path = format!("/file-{size}");
        let inode = entries
            .iter()
            .find(|entry| entry.dir_entry.path().as_bytes() == path.as_bytes())
            .unwrap()
            .inode;
        let tail = size % 4096;
        let layout = if tail != 0 && tail <= 4032 {
            Layout::FlatInline
        } else {
            Layout::FlatPlain
        };
        assert_eq!(inode.layout(), Some(layout), "{path}");
        let mut actual = Vec::new();
        fs.open(&path).unwrap().read_to_end(&mut actual).unwrap();
        assert_eq!(
            actual,
            (0..size).map(|n| (n % 251) as u8).collect::<Vec<_>>()
        );
    }
    for (path, size, layout) in [
        (b"/exact".as_slice(), 4032, Layout::FlatInline),
        (b"/plain", 4045, Layout::FlatPlain),
        (b"/multi", 4172, Layout::FlatInline),
    ] {
        let inode = entries
            .iter()
            .find(|entry| entry.dir_entry.path().as_bytes() == path)
            .unwrap()
            .inode;
        assert_eq!(inode.data_size(), size);
        assert_eq!(inode.layout(), Some(layout));
    }
    for entry in &entries {
        if entry.inode.is_symlink() {
            let size = entry.inode.data_size() as usize;
            assert_eq!(
                fs.read_link_inode(entry.inode).unwrap().as_bytes(),
                vec![b'x'; size]
            );
            assert_eq!(
                entry.inode.layout(),
                Some(if size == 4032 {
                    Layout::FlatInline
                } else {
                    Layout::FlatPlain
                })
            );
        }
        if entry.inode.layout() == Some(Layout::FlatInline) {
            let tail = (entry.inode.data_size() - 1) % 4096 + 1;
            assert!((entry.inode.id() * 32 + 64) % 4096 + tail <= 4096);
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
    // Twelve 320-byte records per block, plus the superblock/root block.
    assert_eq!(image.len(), 11 * 4096);
    let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
    let entries = fs
        .walk_dir("/")
        .unwrap()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    for entry in entries {
        let mut bytes = Vec::new();
        fs.open_inode_file(entry.inode)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, [42; 256]);
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
        let destination = temp.0.join("image.erofs");
        from_directory(&root, &destination).unwrap();
        let image = fs::read(&destination).unwrap();
        let again = temp.0.join("again.erofs");
        from_directory(&root, &again).unwrap();
        assert_eq!(image, fs::read(again).unwrap());
        let base = (&image[1064..]).get_u32_le() as usize * 4096;
        let source = Source {
            data: SliceImage::new(&image),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        assert_eq!(
            fs.get_inode((encode::ROOT_OFFSET / 32) as u64)
                .unwrap()
                .nlink(),
            4
        );
        let entries = fs
            .walk_dir("/")
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
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
            assert_eq!(inode.uid(), metadata.uid());
            assert_eq!(inode.gid(), metadata.gid());
            assert_eq!(inode.permissions().mode(), metadata.mode() & 0o7777);
            assert_eq!(
                inode.modified_unix(),
                (metadata.mtime(), metadata.mtime_nsec() as u32)
            );
            let at = base + inode.id() as usize * 32;
            assert_eq!(
                &image[at..at + 4],
                &[1 | ((inode.layout().unwrap() as u8) << 1), 0, 0, 0]
            );
            assert_eq!(&image[at + 8..at + 16], &inode.data_size().to_le_bytes());
            if inode.is_file() {
                let expected = fs::read(&disk).unwrap();
                let mut file = fs.open_inode_file(inode).unwrap();
                let mut afile = afs.open_inode_file(aentry.inode).unwrap();
                check_read_at(&file, &afile, &expected);
                let mut actual = Vec::new();
                file.read_to_end(&mut actual).unwrap();
                assert_eq!(actual, expected);
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
                assert_eq!(
                    fs.read_link_inode(inode).unwrap().as_bytes(),
                    target.as_os_str().as_bytes()
                );
                assert_eq!(
                    ready(afs.read_link_inode(aentry.inode)).unwrap().as_bytes(),
                    target.as_os_str().as_bytes()
                );
            }
        }
        assert!(ready(aentries.next_entry()).is_none());
        let inode = |path: &[u8]| {
            entries
                .iter()
                .find(|entry| entry.dir_entry.path().as_bytes() == path)
                .unwrap()
                .inode
        };
        let original = inode(b"/file-65553");
        assert_eq!(original.nlink(), 2);
        assert_eq!(original.id(), inode(b"/dir/alias").id());
        assert_eq!(inode(b"/link-alias").nlink(), 2);
        assert_eq!(inode(b"/link-alias").id(), inode(b"/dir/link").id());
        assert!(fs.read_dir("/empty-dir").unwrap().next().is_none());
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
        rustix::fs::setxattr(&file, "user.key", b"value", rustix::fs::XattrFlags::empty()).unwrap();
        assert!(matches!(
            from_directory(&root, &output),
            Err(Error::NotSupported(_))
        ));
        assert!(!output.exists());
        rustix::fs::removexattr(&file, "user.key").unwrap();
        let sources = scan(&root).unwrap();
        fs::write(&file, b"changed length").unwrap();
        assert!(write_image(&sources, &mut Cursor::new(Vec::new())).is_err());
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
    use core::{
        future::{Future, poll_fn},
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use std::cell::Cell;

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

    struct AsyncOutput {
        inner: Output,
        ready: Cell<bool>,
        freeze_at: Option<u64>,
        fail_end: bool,
    }

    impl AsyncOutput {
        fn new() -> Self {
            Self {
                inner: Output {
                    data: Cursor::new(Vec::new()),
                    remaining: usize::MAX,
                    fail_seek: false,
                    fail_flush: false,
                },
                ready: Cell::new(false),
                freeze_at: None,
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

    #[test]
    fn async_and_sync_share_format_and_stream_through_pending_io() {
        let metadata = Metadata {
            mode: 0o6751,
            uid: 65536,
            gid: 70000,
            modified: (-2, 123456789),
        };
        let mut entries: Vec<_> = [
            0, 1, 4095, 4096, 4097, 65553, 31, 32, 4031, 4032, 4033, 8192, 12224, 12225,
        ]
        .into_iter()
        .map(|size| {
            (
                format!("dir/file-{size}").into_bytes(),
                (0..size).map(|n| (n % 251) as u8).collect::<Vec<_>>(),
            )
        })
        .collect();
        entries.push((b"!\xff".to_vec(), b"raw".to_vec()));
        for n in 0..80 {
            entries.push((
                format!("dir/{n:03}{}", "x".repeat(252)).into_bytes(),
                Vec::new(),
            ));
        }
        let mut sync = Builder::new(Cursor::new(Vec::new())).unwrap();
        let mut asynchronous = complete(AsyncBuilder::new(AsyncOutput::new())).unwrap();
        for (path, data) in &entries {
            sync.append_file(
                path.as_slice(),
                metadata,
                data.len() as u64,
                data.as_slice(),
            )
            .unwrap();
            let mut source = Reader {
                data: Cursor::new([data.as_slice(), b"suffix"].concat()),
                ready: Cell::new(false),
                fail: false,
            };
            complete(asynchronous.append_file(
                path.as_slice(),
                metadata,
                data.len() as u64,
                &mut source,
            ))
            .unwrap();
            assert_eq!(source.data.position(), data.len() as u64);
        }
        sync.append_symlink("link", metadata, b"../\xff".as_slice())
            .unwrap();
        complete(asynchronous.append_symlink("link", metadata, b"../\xff".as_slice())).unwrap();
        for (path, target) in [("alias", "dir/file-65553"), ("link-alias", "link")] {
            sync.append_hard_link(path, target).unwrap();
            asynchronous.append_hard_link(path, target).unwrap();
        }
        for path in ["/", "dir/"] {
            sync.append_dir(path, metadata).unwrap();
            asynchronous.append_dir(path, metadata).unwrap();
        }
        let bytes = complete(asynchronous.finish())
            .unwrap()
            .inner
            .data
            .into_inner();
        assert_eq!(bytes, sync.finish().unwrap().into_inner());
        let fs = crate::EroFS::new(SliceImage::new(&bytes)).unwrap();
        assert_eq!(
            fs.get_inode((encode::ROOT_OFFSET / 32) as u64)
                .unwrap()
                .modified_unix(),
            metadata.modified
        );
        for (path, expected) in &entries {
            let mut actual = Vec::new();
            fs.open(path.as_slice())
                .unwrap()
                .read_to_end(&mut actual)
                .unwrap();
            assert_eq!(&actual, expected);
        }
        let at = encode::ROOT_OFFSET;
        assert_eq!(&bytes[at + 24..at + 28], &65536u32.to_le_bytes());
        assert_eq!(&bytes[at + 32..at + 40], &(-2i64).to_le_bytes());

        // Existing async EROFS files are stream inputs directly, with no Tokio adapter.
        let source = Source {
            data: SliceImage::new(&bytes),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = complete(crate::r#async::EroFS::new(&source)).unwrap();
        let mut file = complete(fs.open("dir/file-65553")).unwrap();
        let mut rewritten = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
        complete(rewritten.append_file("copy", metadata, file.size(), &mut file)).unwrap();
        assert_eq!(complete(file.read(&mut [0; 1])).unwrap(), 0);
        let image = complete(rewritten.finish()).unwrap().into_inner();
        let fs = crate::EroFS::new(SliceImage::new(&image)).unwrap();
        let mut actual = Vec::new();
        fs.open("copy").unwrap().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, entries[5].1);
    }

    #[test]
    fn async_validation_does_not_poison_builder() {
        let metadata = Metadata::default();
        let mut builder = complete(AsyncBuilder::new(Cursor::new(Vec::new()))).unwrap();
        for path in ["../bad", "a//b", "a/", "a\0b"] {
            assert!(complete(builder.append_file(path, metadata, 1, Unread)).is_err());
        }
        assert!(complete(builder.append_file("huge", metadata, u64::MAX, Unread)).is_err());
        assert!(
            complete(builder.append_file(
                "bad",
                Metadata {
                    modified: (0, 1_000_000_000),
                    ..metadata
                },
                1,
                Unread
            ))
            .is_err()
        );
        assert!(complete(builder.append_symlink("link", metadata, "")).is_err());
        assert!(complete(builder.append_symlink("link", metadata, "a\0b")).is_err());
        {
            let _unpolled = builder.append_file("not-added", metadata, 1, Unread);
        }
        complete(builder.append_file("empty", metadata, 0, Unread)).unwrap();
        assert!(complete(builder.append_file("/empty", metadata, 1, Unread)).is_err());
        assert!(builder.append_dir("empty/child", metadata).is_err());
        builder.append_dir("dir", metadata).unwrap();
        assert!(builder.append_hard_link("bad", "dir").is_err());
        assert!(builder.append_hard_link("bad", "unknown").is_err());
        complete(builder.finish()).unwrap();
        let mut existing = Cursor::new(b"keep".to_vec());
        assert!(complete(AsyncBuilder::new(&mut existing)).is_err());
        assert_eq!(existing.into_inner(), b"keep");
        let mut output = Cursor::new(Vec::new());
        let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        complete(builder.append_file("missing/file", metadata, 0, Unread)).unwrap();
        assert!(matches!(
            complete(builder.finish()),
            Err(Error::PathNotFound(_))
        ));
        assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
    }

    #[test]
    fn async_io_failures_poison_builder() {
        let metadata = Metadata::default();
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

        let metadata = Metadata::default();
        let mut output = Cursor::new(Vec::new());
        let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        let mut source = PendingTail(&[42; 4097]);
        {
            // The full external block and one inline byte are consumed before
            // the source stalls on the remaining inline byte.
            let mut append = pin!(builder.append_file("cancelled", metadata, 4098, &mut source));
            assert!(
                append
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        assert!(source.0.is_empty());
        assert!(builder.append_dir("later", metadata).is_err());
        assert!(complete(builder.finish()).is_err());
        assert_eq!(&output.get_ref()[8192..], &[42; 4096]);
        assert_eq!(&output.get_ref()[1024..1028], &[0; 4]);
    }

    #[test]
    fn async_cancellation_poisons_builder() {
        let metadata = Metadata::default();
        // Cancel a polled append after a partial payload or during padding.
        for (size, stop) in [(4096, 8193), (4033, 8192 + 4033)] {
            let mut output = AsyncOutput::new();
            output.freeze_at = Some(stop);
            let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
            {
                let mut append =
                    pin!(builder.append_file("cancelled", metadata, size, &[0; 4096][..]));
                for _ in 0..1024 {
                    assert!(
                        append
                            .as_mut()
                            .poll(&mut Context::from_waker(Waker::noop()))
                            .is_pending()
                    );
                }
            }
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
        {
            let mut append = pin!(builder.append_file("next", metadata, 1, Unread));
            for _ in 0..8 {
                assert!(
                    append
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
        }
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
            let mut append = pin!(builder.append_file("cancelled", metadata, 4, source));
            assert!(
                append
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        assert!(builder.append_hard_link("later", "cancelled").is_err());
        assert!(complete(builder.finish()).is_err());

        let mut output = AsyncOutput::new();
        output.freeze_at = Some(4097);
        let mut builder = complete(AsyncBuilder::new(&mut output)).unwrap();
        complete(builder.append_file("empty", metadata, 0, Unread)).unwrap();
        {
            let mut finish = pin!(builder.finish());
            for _ in 0..8 {
                assert!(
                    finish
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
        }
        assert_eq!(output.inner.data.get_ref().len(), 4097);
        assert_eq!(&output.inner.data.get_ref()[1024..1028], &[0; 4]);
        let mut output = AsyncOutput::new();
        output.freeze_at = Some(1);
        {
            let mut initialize = pin!(AsyncBuilder::new(&mut output));
            for _ in 0..16 {
                assert!(
                    initialize
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
        }
        assert_eq!(output.inner.data.get_ref(), &[0]);
    }
}
