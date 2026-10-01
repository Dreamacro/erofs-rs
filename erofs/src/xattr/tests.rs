use super::*;
use crate::{
    backend::{AsyncImage, Image, SliceImage},
    tests::{Source, ready},
    types::MAGIC_NUMBER,
};
use core::{
    ops::{Bound, RangeBounds},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
};

fn entry(index: u8, name: &[u8], value: &[u8]) -> Vec<u8> {
    let mut data = vec![name.len() as u8, index];
    data.extend_from_slice(&(value.len() as u16).to_le_bytes());
    data.extend_from_slice(name);
    data.extend_from_slice(value);
    data.resize(data.len().next_multiple_of(4), 0);
    data
}

struct Fixture {
    data: Vec<u8>,
    body: usize,
    shared: usize,
    prefixes: usize,
    packed: u64,
}

// Root nid 0 contains /f (nid 1). Prefix records may be either image metadata
// or file data in a flat packed inode. All addresses are derived independently
// of the parser's request state.
fn image(
    inline: &[Vec<u8>],
    shared: &[Vec<u8>],
    prefixes: &[Vec<u8>],
    extended: bool,
    packed: bool,
) -> Fixture {
    let inode_size = if extended { 64 } else { 32 };
    let body_at = 2080 + inode_size;
    let mut shared_data = vec![0; 12];
    let mut body = vec![0; 12];
    body[..4].fill(255); // Enumeration must not mistake the lookup filter for data.
    body[4] = shared.len() as u8;
    for record in shared {
        body.extend_from_slice(&((shared_data.len() / 4) as u32).to_le_bytes());
        shared_data.extend_from_slice(record);
    }
    for record in inline {
        body.extend_from_slice(record);
    }
    if inline.is_empty() && shared.is_empty() {
        body.clear();
    }
    let packed_at = (body_at + body.len()).next_multiple_of(32);
    let root_data = (packed_at + 32).next_multiple_of(512);
    let shared_at = root_data + 512;
    let prefix_at = (shared_at + shared_data.len()).next_multiple_of(512);
    // Start near a block end; a prefix record can cross the next block.
    let prefix_offset = if packed { 508 } else { prefix_at + 508 };
    let mut prefix_data = vec![0; 508];
    for prefix in prefixes {
        prefix_data.extend_from_slice(&(prefix.len() as u16).to_le_bytes());
        prefix_data.extend_from_slice(prefix);
        prefix_data.resize(prefix_data.len().next_multiple_of(4), 0);
    }
    let mut data = vec![0; prefix_at + prefix_data.len()];
    data[1024..1028].copy_from_slice(&MAGIC_NUMBER.to_le_bytes());
    data[1032] = 4; // Name-filter feature enabled.
    data[1036] = 9;
    data[1064] = 4;
    data[1068..1072].copy_from_slice(&((shared_at / 512) as u32).to_le_bytes());
    if !prefixes.is_empty() {
        data[1104] = 0x40;
        data[1032] |= if packed { 0 } else { 0x10 };
        data[1115] = prefixes.len() as u8;
        data[1116..1120].copy_from_slice(&((prefix_offset / 4) as u32).to_le_bytes());
    }
    let packed_nid = ((packed_at - 2048) / 32) as u64;
    if packed {
        data[1120..1128].copy_from_slice(&packed_nid.to_le_bytes());
        data[packed_at + 4..packed_at + 6].copy_from_slice(&0o100644u16.to_le_bytes());
        data[packed_at + 8..packed_at + 12]
            .copy_from_slice(&(prefix_data.len() as u32).to_le_bytes());
        data[packed_at + 16..packed_at + 20]
            .copy_from_slice(&((prefix_at / 512) as u32).to_le_bytes());
    }
    data[2048] = 0x10; // Omit dot; retain the parent entry.
    data[2052..2054].copy_from_slice(&0o40755u16.to_le_bytes());
    data[2056..2060].copy_from_slice(&27u32.to_le_bytes());
    data[2064..2068].copy_from_slice(&((root_data / 512) as u32).to_le_bytes());
    data[root_data + 8..root_data + 10].copy_from_slice(&24u16.to_le_bytes());
    data[root_data + 12] = 1;
    data[root_data + 20..root_data + 22].copy_from_slice(&26u16.to_le_bytes());
    data[root_data + 24..root_data + 27].copy_from_slice(b"..f");
    data[2080] = u8::from(extended);
    data[2084..2086].copy_from_slice(&0o100644u16.to_le_bytes());
    if !body.is_empty() {
        data[2082..2084].copy_from_slice(&(((body.len() - 12) / 4 + 1) as u16).to_le_bytes());
    }
    data[body_at..body_at + body.len()].copy_from_slice(&body);
    data[shared_at..shared_at + shared_data.len()].copy_from_slice(&shared_data);
    data[prefix_at..].copy_from_slice(&prefix_data);
    Fixture {
        data,
        body: body_at,
        shared: shared_at + 12,
        prefixes: prefix_at + 508,
        packed: packed_nid,
    }
}

fn check(data: &[u8], expected: &[(&[u8], &[u8])]) {
    let source = Source {
        data: SliceImage::new(data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let expected: Xattrs = expected
        .iter()
        .map(|&(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
    assert_eq!(fs.xattrs("/f").unwrap(), expected);
    assert_eq!(ready(afs.xattrs("/f")).unwrap(), expected);
    let inode = fs.get_inode(1).unwrap();
    assert_eq!(fs.xattrs_inode(inode).unwrap(), expected);
    assert_eq!(ready(afs.xattrs_inode(inode)).unwrap(), expected);
}

fn rejected(data: &[u8]) {
    let source = Source {
        data: SliceImage::new(data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let inode = fs.get_inode(1).unwrap();
    assert!(fs.xattrs_inode(inode).is_err());
    assert!(ready(afs.xattrs_inode(inode)).is_err());
}

#[test]
fn inline_shared_namespaces_and_binary_values() {
    for extended in [false, true] {
        let fixture = image(
            &[
                entry(1, b"\xff/with=bytes\n", b"\0\xff\n=value"),
                entry(1, b"empty", b""),
                entry(2, b"", &[2, 0, 0, 0]),
                entry(0, b"internal", b"hidden"),
                vec![0; 8], // Body padding, not empty user attributes.
            ],
            &[
                entry(4, b"overlay.opaque", b"y"),
                entry(6, b"selinux", b"label\0"),
                entry(6, b"capability", &[1, 0, 0, 0]),
                entry(3, b"", b""), // Zero-sized shared payload; values are not interpreted.
            ],
            &[],
            extended,
            false,
        );
        check(
            &fixture.data,
            &[
                (b"user.\xff/with=bytes\n", b"\0\xff\n=value"),
                (b"user.empty", b""),
                (b"system.posix_acl_access", &[2, 0, 0, 0]),
                (b"system.posix_acl_default", b""),
                (b"trusted.overlay.opaque", b"y"),
                (b"security.selinux", b"label\0"),
                (b"security.capability", &[1, 0, 0, 0]),
            ],
        );
    }
}

#[test]
fn long_prefixes_in_image_and_packed_inode() {
    for packed in [false, true] {
        let prefixes = [
            b"\x01long.\xff.".to_vec(),
            b"\x06selinux".to_vec(),
            b"\0internal".to_vec(),
        ];
        let mut fixture = image(
            &[entry(0x80, b"key", b"one"), entry(0x82, b"", b"hidden")],
            &[entry(0x81, b"", b"two")],
            &prefixes,
            false,
            packed,
        );
        check(
            &fixture.data,
            &[
                (b"user.long.\xff.key", b"one"),
                (b"security.selinux", b"two"),
            ],
        );
        if packed {
            // PLAIN_XATTR_PFX overrides the packed inode even when it is present.
            fixture.data[1032] |= 0x10;
            fixture.data[1116..1120]
                .copy_from_slice(&((fixture.prefixes / 4) as u32).to_le_bytes());
            fixture.data[1120..1128].copy_from_slice(&u64::MAX.to_le_bytes());
        } else {
            // Older images without a packed inode also store prefixes directly.
            fixture.data[1032] &= !0x10;
        }
        check(
            &fixture.data,
            &[
                (b"user.long.\xff.key", b"one"),
                (b"security.selinux", b"two"),
            ],
        );
    }
    // The last representable prefix ID, and a full 255-byte name with no
    // per-entry suffix. Unused namespace-zero prefix records remain hidden.
    let mut prefixes = vec![vec![0]; 128];
    prefixes[127] = [vec![1], vec![b'x'; 250]].concat();
    let name = [b"user.".as_slice(), &prefixes[127][1..]].concat();
    let fixture = image(&[entry(0xff, b"", b"last")], &[], &prefixes, false, true);
    check(&fixture.data, &[(&name, b"last")]);
}

#[test]
fn compressed_prefixes_reuse_extent_mapping() {
    let mut fixture = image(
        &[entry(0x80, b"key", b"one")],
        &[entry(0x81, b"", b"two")],
        &[b"\x01long.".to_vec(), b"\x06selinux".to_vec()],
        false,
        true,
    );
    // Independent liblz4 sample: 508 zeros and the two aligned prefix records.
    const ENCODED: &[u8] = &[
        31, 0, 1, 0, 255, 233, 240, 5, 6, 0, 1, 108, 111, 110, 103, 46, 8, 0, 6, 115, 101, 108,
        105, 110, 117, 120, 0, 0,
    ];
    let inode = 2048 + fixture.packed as usize * 32;
    let physical = fixture.prefixes - 508;
    fixture.data[1104] |= 1; // Leading-zero-padded LZ4.
    fixture.data[inode] = 2; // Compact inode with Full compression indexes.
    fixture.data[inode + 32..inode + 64].fill(0);
    fixture.data[inode + 48] = 1;
    fixture.data[inode + 52..inode + 56].copy_from_slice(&((physical / 512) as u32).to_le_bytes());
    fixture.data[inode + 56] = 2;
    fixture.data[inode + 60] = 1;
    fixture.data[physical..physical + 512].fill(0);
    fixture.data[physical + 512 - ENCODED.len()..physical + 512].copy_from_slice(ENCODED);
    if cfg!(feature = "lz4") {
        check(
            &fixture.data,
            &[(b"user.long.key", b"one"), (b"security.selinux", b"two")],
        );
    } else {
        let source = Source {
            data: SliceImage::new(&fixture.data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        let inode = fs.get_inode(1).unwrap();
        assert!(matches!(
            fs.xattrs_inode(inode),
            Err(Error::NotSupported(_))
        ));
        assert!(matches!(
            ready(afs.xattrs_inode(inode)),
            Err(Error::NotSupported(_))
        ));
    }
}

#[test]
fn shared_id_array_can_span_blocks() {
    let shared: Vec<_> = (0..255)
        .map(|i| entry(1, format!("key{i}").as_bytes(), &[i]))
        .collect();
    let fixture = image(&[entry(1, b"inline", b"last")], &shared, &[], false, false);
    let mut expected: Xattrs = (0..255)
        .map(|i| (format!("user.key{i}").into_bytes(), vec![i]))
        .collect();
    expected.insert(b"user.inline".to_vec(), b"last".to_vec());
    let pairs: Vec<_> = expected
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect();
    check(&fixture.data, &pairs);
}

#[test]
fn no_attributes_do_not_read_and_failures_can_be_retried() {
    let empty = image(&[], &[], &[], false, false);
    let source = Source {
        data: SliceImage::new(&empty.data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::EroFS::new(&source).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let inode = fs.get_inode(1).unwrap();
    source.fail.store(true, Relaxed);
    let reads = source.reads.load(Relaxed);
    assert!(fs.xattrs_inode(inode).unwrap().is_empty());
    assert!(ready(afs.xattrs_inode(inode)).unwrap().is_empty());
    assert_eq!(source.reads.load(Relaxed), reads);

    let mut fixture = image(&[entry(1, b"test", b"ok")], &[], &[], false, false);
    for mode in [0o100644u16, 0o40755, 0o120777, 0o20600, 0o10600, 0o140600] {
        fixture.data[2084..2086].copy_from_slice(&mode.to_le_bytes());
        let source = Source {
            data: SliceImage::new(&fixture.data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        let inode = fs.get_inode(1).unwrap();
        source.fail.store(true, Relaxed);
        assert!(fs.xattrs_inode(inode).is_err());
        assert!(ready(afs.xattrs_inode(inode)).is_err());
        source.fail.store(false, Relaxed);
        assert_eq!(
            fs.xattrs("/f")
                .unwrap()
                .get(b"user.test".as_slice())
                .unwrap(),
            b"ok"
        );
        assert_eq!(
            ready(afs.xattrs("/f"))
                .unwrap()
                .get(b"user.test".as_slice())
                .unwrap(),
            b"ok"
        );
        assert!(matches!(fs.xattrs("/missing"), Err(Error::PathNotFound(_))));
        assert!(matches!(
            ready(afs.xattrs("/missing")),
            Err(Error::PathNotFound(_))
        ));
    }
    // Attribute access does not try to decode the subject inode's file data.
    fixture.data[2080] = 2;
    fixture.data[2084..2086].copy_from_slice(&0o100644u16.to_le_bytes());
    fixture.data[2088..2092].copy_from_slice(&100_000u32.to_le_bytes());
    check(&fixture.data, &[(b"user.test", b"ok")]);
}

#[test]
fn maximum_value_and_name_sizes_cross_metadata_blocks() {
    let suffix = vec![b'a'; 250];
    let name = [b"user.".as_slice(), &suffix].concat();
    let value: Vec<u8> = (0..65535).map(|i| i as u8).collect();
    for shared in [false, true] {
        let record = entry(1, &suffix, &value);
        let fixture = if shared {
            image(&[], &[record], &[], true, false)
        } else {
            image(&[record], &[], &[], true, false)
        };
        check(&fixture.data, &[(&name, &value)]);
    }
}

#[test]
fn invalid_headers_entries_prefixes_and_duplicates_are_rejected() {
    let base = image(&[entry(1, b"key", b"value")], &[], &[], false, false);
    for (offset, value) in [
        (base.body + 4, 255),
        (base.body + 5, 1),
        (base.body + 12, 255),
        (base.body + 13, 7),
        (base.body + 14, 255),
        (base.body + 16, 0),
    ] {
        let mut data = base.data.clone();
        data[offset] = value;
        rejected(&data);
    }
    let mut data = base.data.clone();
    data[2082..2084].copy_from_slice(&1u16.to_le_bytes());
    rejected(&data); // Header-only body has no defined format.
    rejected(&base.data[..base.body + 15]);
    for record in [
        entry(1, b"", b"bad"),
        entry(2, b"suffix", b"bad"),
        entry(1, &[b'a'; 251], b"bad"),
        entry(0x80, b"key", b"bad"),
    ] {
        rejected(&image(&[record], &[], &[], false, false).data);
    }
    let duplicate = entry(1, b"key", b"other");
    rejected(
        &image(
            &[entry(1, b"key", b"value"), duplicate.clone()],
            &[],
            &[],
            false,
            false,
        )
        .data,
    );
    rejected(
        &image(
            &[entry(1, b"key", b"value")],
            &[duplicate],
            &[],
            false,
            false,
        )
        .data,
    );
    let shared = image(&[], &[entry(1, b"key", b"value")], &[], false, false);
    rejected(&shared.data[..shared.shared + 4 + 3 + 4]);
    let mut data = shared.data;
    data[1032] |= 8; // Shared EA metadata in metabox is not raw image metadata.
    rejected(&data);

    for prefix in [
        b"\x01bad\0".to_vec(),
        b"\x02suffix".to_vec(),
        b"\x07unknown".to_vec(),
        vec![1; 257],
    ] {
        rejected(
            &image(
                &[entry(0x80, b"key", b"value")],
                &[],
                &[prefix],
                false,
                false,
            )
            .data,
        );
    }
    let base = image(
        &[entry(0x80, b"key", b"value")],
        &[],
        &[b"\x01long.".to_vec()],
        false,
        false,
    );
    for (offset, value) in [
        (1104, 0),
        (1115, 129),
        (base.prefixes, 0),
        (base.body + 13, 0x81),
    ] {
        let mut data = base.data.clone();
        data[offset] = value;
        rejected(&data);
    }
    rejected(&base.data[..base.prefixes + 3]);
    let base = image(
        &[entry(0x80, b"key", b"value")],
        &[],
        &[b"\x01long.".to_vec()],
        false,
        true,
    );
    let packed = 2048 + base.packed as usize * 32;
    let mut data = base.data.clone();
    data[packed + 4..packed + 6].copy_from_slice(&0o40755u16.to_le_bytes());
    rejected(&data);
    let mut data = base.data;
    data[packed + 8..packed + 12].copy_from_slice(&510u32.to_le_bytes());
    rejected(&data);
}

struct Sparse<'a>(Vec<(u64, &'a [u8])>);
impl Image for &Sparse<'_> {
    fn len(&self) -> u64 {
        u64::MAX
    }
    fn get<R: RangeBounds<u64>>(&self, range: R) -> Option<&[u8]> {
        let (Bound::Included(&start), Bound::Excluded(&end)) =
            (range.start_bound(), range.end_bound())
        else {
            panic!("bounded reads expected")
        };
        self.0.iter().find_map(|&(offset, data)| {
            let start = usize::try_from(start.checked_sub(offset)?).ok()?;
            let end = usize::try_from(end.checked_sub(offset)?).ok()?;
            data.get(start..end)
        })
    }
}
impl AsyncImage for &Sparse<'_> {
    async fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        let data = self
            .get(offset..offset + buf.len() as u64)
            .ok_or_else(|| Error::OutOfBounds("sparse test read".into()))?;
        buf.copy_from_slice(data);
        Ok(())
    }
}

#[test]
fn metadata_addresses_remain_u64() {
    let mut fixture = image(
        &[entry(0x80, b"inline", b"one")],
        &[entry(1, b"shared", b"two")],
        &[b"\x01long.".to_vec()],
        false,
        false,
    );
    fixture.data[1064..1068].copy_from_slice(&0x80000000u32.to_le_bytes());
    fixture.data[1068..1072].copy_from_slice(&0x90000000u32.to_le_bytes());
    fixture.data[1116..1120].copy_from_slice(&0xa0000000u32.to_le_bytes());
    let image = Sparse(vec![
        (1024, &fixture.data[1024..1152]),
        (0x80000000u64 * 512, &fixture.data[2048..fixture.body + 40]),
        (
            0x90000000u64 * 512 + 12,
            &fixture.data[fixture.shared..fixture.shared + 16],
        ),
        (0xa0000000u64 * 4, &fixture.data[fixture.prefixes..]),
    ]);
    let fs = crate::EroFS::new(&image).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&image)).unwrap();
    let inode = fs.get_inode(1).unwrap();
    let expected: Xattrs = [
        (b"user.long.inline".to_vec(), b"one".to_vec()),
        (b"user.shared".to_vec(), b"two".to_vec()),
    ]
    .into();
    assert_eq!(fs.xattrs_inode(inode).unwrap(), expected);
    assert_eq!(ready(afs.xattrs_inode(inode)).unwrap(), expected);
}
