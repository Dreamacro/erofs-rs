use super::*;
use crate::{
    backend::SliceImage,
    tests::{Read, Source, directory, ready},
    types::MAGIC_NUMBER,
};
use alloc::{borrow::Cow, vec::Vec};
use bytes::BufMut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

fn inode(data: &mut [u8], at: usize, format: u16, mode: u16, size: u32, block: u32) {
    (&mut data[at..]).put_u16_le(format);
    let mut fields = &mut data[at + 4..];
    fields.put_u16_le(mode);
    fields.put_u16_le(1);
    fields.put_u32_le(size);
    (&mut data[at + 16..]).put_u32_le(block);
}

// A literal-only LZ4 block needs no encoder dependency.
fn literals(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0xf0];
    let mut length = data.len() - 15;
    while length >= 255 {
        out.put_u8(255);
        length -= 255;
    }
    out.put_u8(length as u8);
    out.put_slice(data);
    out
}

fn metadata() -> Vec<u8> {
    let mut data = vec![0; 2048];
    inode(&mut data, 0, 4, 0o40755, 224, 0);
    directory(
        &mut data[32..256],
        &[
            (u64::MAX, b".", 2),
            (u64::MAX, b"..", 2),
            (NID_METABOX | 8, b"a", 2),
            (NID_METABOX | 10, b"b", 1),
            (NID_METABOX | 15, b"c", 1),
            (NID_METABOX | 20, b"d", 1),
            (NID_METABOX | 18, b"f", 1),
            (8, b"p", 1),
            (NID_METABOX | 24, b"x", 1),
        ],
    );
    inode(&mut data, 256, 4, 0o100644, 17, 0);
    data[288..305].fill(b'a');
    inode(&mut data, 320, 8, 0o100644, 700, 0x20);
    (&mut data[356..]).put_u32_le(20);
    (&mut data[364..]).put_u32_le(21);
    // Extended inode straddles two metabox extents.
    inode(&mut data, 480, 1, 0o100644, 700, 20);
    (&mut data[486..]).put_u16_le(0);
    (&mut data[524..]).put_u32_le(1);
    inode(&mut data, 576, 2, 0o100644, 17, 0);
    (&mut data[608..]).put_u64_le(NID_METABOX | 5);
    inode(&mut data, 640, 2, 0o100644, 20, 0);
    let encoded = literals(&[b'Z'; 20]);
    (&mut data[674..]).put_u16_le(encoded.len() as u16);
    (&mut data[676..]).put_u16_le(8);
    (&mut data[688..]).put_u16_le(1); // Full-index HEAD1, followed by inline compressed bytes.
    data[696..696 + encoded.len()].copy_from_slice(&encoded);
    inode(&mut data, 768, 0, 0o100644, 0, 0);
    (&mut data[770..]).put_u16_le(254); // 1024-byte xattr body spans metabox extents.
    data[804] = 1;
    (&mut data[812..]).put_u32_le(475);
    data[816] = 3;
    data[817] = 0x80;
    (&mut data[818..]).put_u16_le(1000);
    data[820..823].copy_from_slice(b"key");
    data[823..1823].fill(b'V');
    (&mut data[1888..]).put_u16_le(6);
    data[1890..1896].copy_from_slice(b"\x01meta.");
    data[1900] = 7;
    data[1901] = 6;
    (&mut data[1902..]).put_u16_le(3);
    data[1904..1914].copy_from_slice(b"selinuxyes");
    data
}

fn image(meta: &[u8], compressed: bool) -> Vec<u8> {
    let mut data = vec![0; 16384];
    (&mut data[1024..]).put_u32_le(MAGIC_NUMBER);
    data[1036] = 9;
    data[1037] = 1;
    (&mut data[1032..]).put_u32_le(8);
    (&mut data[1060..]).put_u32_le(32);
    (&mut data[1064..]).put_u32_le(4);
    (&mut data[1104..]).put_u32_le(0x1f3);
    (&mut data[1108..]).put_u16_le(1);
    data[1115] = 1;
    (&mut data[1116..]).put_u32_le(1888 / 4);
    (&mut data[1120..]).put_u64_le(12);
    (&mut data[1136..]).put_u64_le(NID_METABOX);
    (&mut data[1152..]).put_u64_le(4); // Metabox inode 4 is in primary metadata.
    (&mut data[1168..]).put_u16_le(14); // Global configuration follows the extension slot.
    inode(&mut data, 2176, 2, 0o100644, meta.len() as u32, 0);
    (&mut data[2208..]).put_u32_le(3);
    (&mut data[2212..]).put_u16_le(5); // Three 16-byte extents.
    for (i, (start, end, block)) in [(0, 512, 8), (512, 1024, 16), (1024, 2048, 24)]
        .into_iter()
        .enumerate()
    {
        let bytes = &meta[start..end];
        let payload = if compressed {
            literals(bytes)
        } else {
            bytes.to_vec()
        };
        let size = payload.len().next_multiple_of(512);
        let at = 2224 + i * 16;
        (&mut data[at..]).put_u32_le(size as u32 | if compressed { 1 << 28 } else { 0 });
        (&mut data[at + 4..]).put_u32_le(block * 512);
        (&mut data[at + 12..]).put_u32_le(start as u32);
        let end = block as usize * 512 + size;
        data[end - payload.len()..end].copy_from_slice(&payload);
    }
    inode(&mut data, 2304, 0, 0o100644, 700, 20); // Ordinary nid 8 != metabox nid 8.
    inode(&mut data, 2432, 0, 0o100644, 2048, 28);
    data[10240..10752].fill(b'A');
    data[10752..10940].fill(b'B');
    data[14336..16384].fill(b'P');
    (&mut data[16224..]).put_u16_le(6);
    data[16226..16232].copy_from_slice(b"\x01pack.");
    (&mut data[1888..]).put_u16_le(6);
    data[1890..1896].copy_from_slice(b"\x01disk.");
    data
}

fn check(data: &[u8], extras: &[&[u8]], prefix: &[u8]) {
    let fs = crate::EroFS::new_with_devices(
        SliceImage::new(data),
        extras.iter().map(|d| SliceImage::new(d)).collect(),
    )
    .unwrap();
    let sources: Vec<_> = core::iter::once(data)
        .chain(extras.iter().copied())
        .map(|d| Source {
            data: SliceImage::new(d),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        })
        .collect();
    let afs = ready(crate::r#async::EroFS::new_with_devices(
        &sources[0],
        sources[1..].iter().collect(),
    ))
    .unwrap();
    fn send(_: impl core::future::Future + Send) {}
    send(afs.get_inode(NID_METABOX));
    send(afs.xattrs("/x"));
    send(afs.open("/a"));
    let mut seen = Vec::new();
    let mut adir = ready(afs.walk_dir("/")).unwrap();
    for entry in fs.walk_dir("/").unwrap() {
        let entry = entry.unwrap();
        let aentry = ready(adir.next_entry()).unwrap().unwrap();
        assert_eq!(entry.inode.id(), aentry.inode.id());
        let name = entry.dir_entry.file_name();
        seen.push(name.to_vec());
        let expected = match name {
            b"a" => vec![b'a'; 17],
            b"f" => vec![b'P'; 17],
            b"x" => Vec::new(),
            b"d" => vec![b'Z'; 20],
            _ => [vec![b'A'; 512], vec![b'B'; 188]].concat(),
        };
        assert_eq!(entry.inode.id() & NID_METABOX != 0, name != b"p");
        let mut file = fs.open_inode_file(entry.inode).unwrap();
        let mut afile = afs.open_inode_file(aentry.inode).unwrap();
        if name == b"d" && !cfg!(feature = "lz4") {
            assert!(file.read(&mut [0; 1]).is_err());
            assert!(ready(afile.read(&mut [0; 1])).is_err());
            continue;
        }
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut buf = [0; 113];
        loop {
            let n = file.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            left.put_slice(&buf[..n]);
        }
        loop {
            let n = ready(afile.read(&mut buf[..7])).unwrap();
            if n == 0 {
                break;
            }
            right.put_slice(&buf[..n]);
        }
        assert_eq!(left, expected);
        assert_eq!(right, expected);
    }
    assert_eq!(seen, [b"a", b"b", b"c", b"d", b"f", b"p", b"x"]);
    assert!(ready(adir.next_entry()).is_none());
    let attrs = fs.xattrs("/x").unwrap();
    assert_eq!(attrs, ready(afs.xattrs("/x")).unwrap());
    assert_eq!(attrs.len(), 2);
    assert_eq!(attrs[prefix], vec![b'V'; 1000]);
    assert_eq!(attrs[b"security.selinux".as_slice()], b"yes");
    // Ordinary data keeps borrowing even when its inode lives in the metabox.
    let inode = fs.get_inode(NID_METABOX | 15).unwrap();
    assert!(matches!(
        fs.get_inode_data(&inode, 0).unwrap(),
        Cow::Borrowed(_)
    ));
}

#[test]
fn metadata_sources_cover_layouts_xattrs_and_devices() {
    for compressed in [false, true] {
        let mut data = image(&metadata(), compressed);
        if compressed && !cfg!(feature = "lz4") {
            let fs = crate::EroFS::new(SliceImage::new(&data)).unwrap();
            assert!(matches!(
                fs.get_inode(NID_METABOX),
                Err(Error::NotSupported(_))
            ));
            continue;
        }
        check(&data, &[], b"user.meta.key");
        data[1032] |= 0x10;
        check(&data, &[], b"user.disk.key");
        data[1032] &= !0x10;
        // Move all data, including the metabox contents, onto an additional device.
        data[1104] |= 8;
        (&mut data[1110..]).put_u16_le(1);
        (&mut data[1112..]).put_u16_le(10);
        (&mut data[1280 + 64..]).put_u32_le(24);
        (&mut data[1280 + 68..]).put_u32_le(8);
        (&mut data[1060..]).put_u32_le(8);
        let blob = data.split_off(4096);
        check(&data, &[&blob], b"user.meta.key");
    }
}

#[test]
fn extensions_ranges_and_bootstrap_are_checked() {
    assert_eq!(SuperBlock::size(), 128);
    let valid = image(&metadata(), false);
    for (at, bytes) in [
        (1037, vec![0]),
        (1152, (NID_METABOX | 1).to_le_bytes().to_vec()),
        (1120, (NID_METABOX | 1).to_le_bytes().to_vec()),
    ] {
        let mut data = valid.clone();
        data[at..at + bytes.len()].copy_from_slice(&bytes);
        let source = Source {
            data: SliceImage::new(&data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        assert!(crate::EroFS::new(&source).is_err());
        assert!(ready(crate::r#async::EroFS::new(&source)).is_err());
    }
    for end in [1152, 1159, 1167] {
        let source = Source {
            data: SliceImage::new(&valid[..end]),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        assert!(crate::EroFS::new(&source).is_err());
        assert!(ready(crate::r#async::EroFS::new(&source)).is_err());
    }
    for (at, bytes) in [
        (2180, 0o40755u16.to_le_bytes().to_vec()),
        (2184, 511u32.to_le_bytes().to_vec()),
    ] {
        let mut data = valid.clone();
        data[at..at + bytes.len()].copy_from_slice(&bytes);
        let source = Source {
            data: SliceImage::new(&data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        assert!(fs.get_inode(NID_METABOX | 15).is_err());
        assert!(ready(afs.get_inode(NID_METABOX | 15)).is_err());
    }
    let fs = crate::EroFS::new(SliceImage::new(&valid)).unwrap();
    assert!(fs.get_inode(u64::MAX).is_err());
    // Both direct and metabox -> packed -> packed fragment loops must terminate.
    for packed in [4, 12] {
        let mut data = valid.clone();
        (&mut data[1120..]).put_u64_le(packed);
        (&mut data[2208..]).put_u64_le(NID_METABOX);
        if packed == 12 {
            (&mut data[2432..]).put_u16_le(2);
            (&mut data[2464..]).put_u64_le(NID_METABOX);
        }
        let source = Source {
            data: SliceImage::new(&data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        assert!(
            matches!(fs.get_inode(NID_METABOX), Err(Error::CorruptedData(message)) if message == "invalid packed fragment reference")
        );
        assert!(
            matches!(ready(afs.get_inode(NID_METABOX)), Err(Error::CorruptedData(message)) if message == "invalid packed fragment reference")
        );
    }
}

#[test]
fn failed_metabox_loads_can_be_retried_and_ids_stay_distinct() {
    let data = image(&metadata(), false);
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let mut file = fs.open("/a").unwrap();
    let mut afile = ready(afs.open("/a")).unwrap();
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    assert_eq!(file.read(&mut []).unwrap(), 0);
    assert_eq!(ready(afile.read(&mut [])).unwrap(), 0);
    assert_eq!(source.reads.load(Relaxed), reads);
    assert!(file.read(&mut [0; 7]).is_err());
    assert!(ready(afile.read(&mut [0; 7])).is_err());
    source.fail.store(false, Relaxed);
    let mut bytes = [0; 17];
    assert_eq!(file.read(&mut bytes[..7]).unwrap(), 7);
    assert_eq!(ready(afile.read(&mut bytes[..7])).unwrap(), 7);
    assert_eq!(&bytes[..7], &[b'a'; 7]);
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    assert_eq!(file.read(&mut bytes).unwrap(), 10);
    assert_eq!(ready(afile.read(&mut bytes)).unwrap(), 10);
    assert_eq!(&bytes[..10], &[b'a'; 10]);
    assert_eq!(file.read(&mut bytes).unwrap(), 0);
    assert_eq!(ready(afile.read(&mut bytes)).unwrap(), 0);
    assert_eq!(source.reads.load(Relaxed), reads);

    let mut meta = metadata();
    // A genuine directory cycle uses the full metabox NID, not its low bits.
    (&mut meta[32 + 2 * 12..]).put_u64_le(NID_METABOX);
    let data = image(&meta, false);
    let source = Source {
        data: SliceImage::new(&data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    source.fail.store(true, Relaxed);
    assert!(fs.get_inode(NID_METABOX | 15).is_err());
    assert!(ready(afs.get_inode(NID_METABOX | 15)).is_err());
    source.fail.store(false, Relaxed);
    assert_eq!(fs.get_inode(NID_METABOX | 15).unwrap().data_size(), 700);
    assert_eq!(
        ready(afs.get_inode(NID_METABOX | 15)).unwrap().data_size(),
        700
    );
    assert!(matches!(
        fs.walk_dir("/").unwrap().next().unwrap(),
        Err(Error::CorruptedData(_))
    ));
    assert!(matches!(
        ready(ready(afs.walk_dir("/")).unwrap().next_entry()).unwrap(),
        Err(Error::CorruptedData(_))
    ));
}
