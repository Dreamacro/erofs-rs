use super::*;
use crate::{
    EroFS,
    backend::SliceImage,
    filesystem::{BlockPlan, EroFSCore},
    tests::{Read, Source, directory, image, ready},
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

fn fixture() -> [Vec<u8>; 3] {
    let mut data = image();
    data[1060] = 13; // Primary image blocks.
    data[1104] = 8;
    data[1110] = 2; // Two additional devices, table at byte 1280.
    data[1112] = 10;
    for (slot, start) in [(1280, 4), (1408, 20)] {
        data[slot] = 0xff; // Tags are opaque bytes.
        data[slot + 64] = 4;
        data[slot + 68] = start;
    }
    // Device 1's unified range deliberately overlaps primary metadata.
    data[2088..2092].copy_from_slice(&512u32.to_le_bytes());
    directory(
        &mut data[4096..4608],
        &[(1, b".", 2), (1, b"..", 2), (2, b"a", 1)],
    );
    data[2112] = 8; // Indexed chunks, 1024 bytes per chunk.
    data[2114] = 3; // 20-byte xattr body makes the index require 8-byte alignment.
    data[2120..2124].copy_from_slice(&3089u32.to_le_bytes());
    data[2128..2132].copy_from_slice(&0x21u32.to_le_bytes());
    data[2144..2164].fill(0); // Empty internal xattrs.
    data[2164..2168].fill(0xaa); // Not part of the first index.
    for (index, id, block) in [
        (0, 0xfffeu16, 0u32),
        (1, 0xfffd, 0),
        (2, 0xffff, u32::MAX),
        (3, 0, 21),
    ] {
        let at = 2168 + index * 8;
        data[at..at + 2].copy_from_slice(&0xbeefu16.to_le_bytes()); // Ignored without 48-bit chunks.
        data[at + 2..at + 4].copy_from_slice(&id.to_le_bytes());
        data[at + 4..at + 8].copy_from_slice(&block.to_le_bytes());
    }
    let a = [vec![b'A'; 512], vec![b'B'; 1536]].concat();
    let b = [vec![b'C'; 512], vec![b'D'; 1536]].concat();
    [data, a, b]
}

#[test]
fn indexed_devices_preserve_bytes_borrows_holes_and_retries() {
    for (extended, wide) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut data = fixture();
        let at = if extended {
            data[0].copy_within(2144..2200, 2176);
            data[0][2144..2176].fill(0);
            data[0][2112] |= 1;
            2200
        } else {
            2168
        };
        if wide {
            data[0][2128] |= 0x40; // The chunk flag, not the superblock, selects address width.
            for index in 0..4 {
                data[0][at + index * 8..at + index * 8 + 2].fill(if index == 2 { 0xff } else { 0 });
            }
        }
        let sources = data.each_ref().map(|data| Source {
            data: SliceImage::new(data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let fs = EroFS::new_with_devices(&sources[0], vec![&sources[1], &sources[2]])
            .unwrap()
            .clone();
        let afs = ready(crate::r#async::EroFS::new_with_devices(
            &sources[0],
            vec![&sources[1], &sources[2]],
        ))
        .unwrap();
        assert_eq!(fs.devices(), afs.devices());
        assert_eq!(fs.devices()[0].tag[0], 0xff);
        assert!(fs.xattrs("/a").unwrap().is_empty());
        assert!(ready(afs.xattrs("/a")).unwrap().is_empty());
        let inode = fs.get_inode(2).unwrap();
        let bytes = fs.get_inode_data(&inode, 1541).unwrap();
        assert!(matches!(bytes, alloc::borrow::Cow::Borrowed(_)));
        assert_eq!(bytes.as_ptr(), data[1][517..].as_ptr());
        assert_eq!(&*bytes, &[b'B'; 507]);
        let expected = [
            vec![b'C'; 512],
            vec![b'D'; 512],
            vec![b'A'; 512],
            vec![b'B'; 512],
            vec![0; 1024],
            vec![b'D'; 17],
        ]
        .concat();
        let mut file = fs.open("/a").unwrap();
        let mut afile = ready(afs.open("/a")).unwrap();
        let mut offset = 0;
        let mut buf = [0; 700];
        while offset < expected.len() {
            let n = file.read(&mut buf).unwrap();
            assert!(n > 0);
            assert_eq!(&buf[..n], &expected[offset..offset + n]);
            assert_eq!(ready(afile.read(&mut buf)).unwrap(), n);
            assert_eq!(&buf[..n], &expected[offset..offset + n]);
            offset += n;
        }
        for source in &sources {
            source.fail.store(true, Relaxed);
        }
        assert_eq!(file.read(&mut buf).unwrap(), 0);
        assert_eq!(ready(afile.read(&mut buf)).unwrap(), 0);
        sources[0].fail.store(false, Relaxed);
        let reads = [
            sources[1].reads.load(Relaxed),
            sources[2].reads.load(Relaxed),
        ];
        assert_eq!(&*fs.get_inode_data(&inode, 2048).unwrap(), &[0; 512]);
        assert_eq!(ready(afs.read_inode_data(&inode, 2048)).unwrap(), [0; 512]);
        assert_eq!(
            reads,
            [
                sources[1].reads.load(Relaxed),
                sources[2].reads.load(Relaxed)
            ]
        );
        let mut file = fs.open("/a").unwrap();
        let mut afile = ready(afs.open("/a")).unwrap();
        assert_eq!(file.read(&mut []).unwrap(), 0);
        assert_eq!(ready(afile.read(&mut [])).unwrap(), 0);
        assert!(file.read(&mut buf).is_err());
        assert!(ready(afile.read(&mut buf)).is_err());
        sources[2].fail.store(false, Relaxed);
        assert_eq!(file.read(&mut buf[..17]).unwrap(), 17);
        assert_eq!(&buf[..17], &[b'C'; 17]);
        assert_eq!(ready(afile.read(&mut buf[..17])).unwrap(), 17);
        assert_eq!(&buf[..17], &[b'C'; 17]);
        sources[2].fail.store(true, Relaxed);
        let reads = sources[2].reads.load(Relaxed);
        assert_eq!(file.read(&mut buf).unwrap(), 495);
        assert_eq!(&buf[..495], &[b'C'; 495]);
        assert_eq!(ready(afile.read(&mut buf)).unwrap(), 495);
        assert_eq!(&buf[..495], &[b'C'; 495]);
        assert_eq!(sources[2].reads.load(Relaxed), reads);
    }
}

#[test]
fn unified_flat_data_and_primary_inline_metadata_are_distinct() {
    for inline in [false, true] {
        let [mut data, a, b] = fixture();
        data[2112] = if inline { 4 } else { 0 };
        data[2114] = 0;
        data[2120..2124].copy_from_slice(&700u32.to_le_bytes());
        data[2128..2132].copy_from_slice(&20u32.to_le_bytes());
        data[2144..2332].fill(b'!');
        let sources = [&data, &a, &b].map(|data| Source {
            data: SliceImage::new(data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let fs = EroFS::new_with_devices(&sources[0], vec![&sources[1], &sources[2]]).unwrap();
        let afs = ready(crate::r#async::EroFS::new_with_devices(
            &sources[0],
            vec![&sources[1], &sources[2]],
        ))
        .unwrap();
        let expected = [vec![b'C'; 512], vec![if inline { b'!' } else { b'D' }; 188]].concat();
        let mut file = fs.open("/a").unwrap();
        let mut afile = ready(afs.open("/a")).unwrap();
        for offset in [0, 512] {
            let mut buf = [0; 512];
            let n = file.read(&mut buf).unwrap();
            assert_eq!(&buf[..n], &expected[offset..offset + n]);
            assert_eq!(ready(afile.read(&mut buf)).unwrap(), n);
            assert_eq!(&buf[..n], &expected[offset..offset + n]);
        }
    }
}

#[test]
fn invalid_tables_devices_and_truncated_payloads_fail() {
    let [data, a, b] = fixture();
    for count in [0, 1, 3] {
        assert!(
            EroFS::new_with_devices(
                SliceImage::new(&data),
                (0..count).map(|_| SliceImage::new(&a)).collect()
            )
            .is_err()
        );
        let source = Source {
            data: SliceImage::new(&data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        assert!(
            ready(crate::r#async::EroFS::new_with_devices(
                &source,
                vec![&source; count]
            ))
            .is_err()
        );
    }
    for kind in 0..8 {
        let mut data = data.clone();
        match kind {
            0 => data.truncate(1535),
            1 => data[1476] = 6, // Device ranges overlap.
            2 => data[2170..2172].copy_from_slice(&3u16.to_le_bytes()), // Masked ID is absent.
            3 => data[2172..2176].copy_from_slice(&4u32.to_le_bytes()), // Past device capacity.
            4 => data[2170..2176].fill(0), // Primary address zero is valid.
            5 => data[2172..2176].copy_from_slice(&3u32.to_le_bytes()), // Valid descriptor, short backing file.
            6 => {
                data[2170..2172].fill(0);
                data[2172..2176].copy_from_slice(&15u32.to_le_bytes());
            } // Beyond primary.
            7 => {
                data[2170..2172].fill(0);
                data[2172..2176].copy_from_slice(&7u32.to_le_bytes());
            } // The second block must not escape the chunk's selected device.
            _ => unreachable!(),
        }
        let sources = [data.as_slice(), a.as_slice(), &b[..1024]].map(|data| Source {
            data: SliceImage::new(data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let fs = EroFS::new_with_devices(&sources[0], vec![&sources[1], &sources[2]]);
        let afs = ready(crate::r#async::EroFS::new_with_devices(
            &sources[0],
            vec![&sources[1], &sources[2]],
        ));
        if kind < 2 {
            assert!(fs.is_err());
            assert!(afs.is_err());
        } else {
            let fs = fs.unwrap();
            let afs = afs.unwrap();
            let inode = fs.get_inode(2).unwrap();
            if kind == 4 {
                assert_eq!(&*fs.get_inode_data(&inode, 0).unwrap(), &[0; 512]);
                assert_eq!(ready(afs.read_inode_data(&inode, 0)).unwrap(), [0; 512]);
            } else {
                let offset = if kind == 7 { 512 } else { 0 };
                assert!(fs.get_inode_data(&inode, offset).is_err());
                assert!(ready(afs.read_inode_data(&inode, offset)).is_err());
            }
        }
    }
}

#[test]
fn full_device_id_width_and_zero_unified_addresses() {
    let [mut data, a, b] = fixture();
    let fs = EroFS::new_with_devices(
        SliceImage::new(&data),
        vec![SliceImage::new(&a), SliceImage::new(&b)],
    )
    .unwrap()
    .clone();
    assert_eq!(fs.devices().len(), 2); // SliceImage does not implement Clone.
    for count in [1u16, 3, u16::MAX] {
        data[1110..1112].copy_from_slice(&count.to_le_bytes());
        let mut table = vec![0; usize::from(count) * SLOT_SIZE];
        table[(usize::from(count) - 1) * SLOT_SIZE + 64] = 1;
        let mut core = EroFSCore::new(&data[1024..1152]).unwrap();
        core.set_device_table(&table).unwrap();
        assert!(
            matches!(core.resolve_chunk_read(&[0, 0, 255, 255, 0, 0, 0, 0], 0x20, 0, 0, 1).unwrap(),
            BlockPlan::Direct { device_id, offset: 0, size: 1 } if device_id == count)
        );
        // A zero unified start never redirects primary address zero.
        assert_eq!(core.resolve_device(0, 0, 1).unwrap(), (0, 0));
    }
}

#[test]
fn wide_chunks_holes_and_device_ranges_use_u64() {
    let [mut data, _, _] = fixture();
    data[1104] |= 0x80;
    let high = (1u64 << 40) + 7;
    for (at, high_at, value) in [(1344, 1352, high + 10), (1348, 1354, high + 100)] {
        data[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
        data[high_at..high_at + 2].copy_from_slice(&((value >> 32) as u16).to_le_bytes());
    }
    assert_eq!(
        DeviceTable::parse(&data[1280..1536], 512, 13, false)
            .unwrap()
            .entries[0]
            .blocks,
        u64::from((high + 10) as u32)
    );
    let mut core = EroFSCore::new(&data[1024..1152]).unwrap();
    core.set_device_table(&data[1280..1536]).unwrap();
    assert_eq!(core.devices()[0].blocks, high + 10);
    let mut record = [0; 8];
    record[..2].copy_from_slice(&((high >> 32) as u16).to_le_bytes());
    record[2..4].copy_from_slice(&1u16.to_le_bytes());
    record[4..].copy_from_slice(&(high as u32).to_le_bytes());
    assert!(
        matches!(core.resolve_chunk_read(&record, 0x60, 2, 17, 31).unwrap(),
        BlockPlan::Direct { device_id: 1, offset, size: 31 } if offset == (high + 2) * 512 + 17)
    );
    assert!(
        matches!(core.resolve_chunk_read(&record, 0x20, 0, 0, 1).unwrap(),
        BlockPlan::Direct { device_id: 1, offset, .. } if offset == (high & 0xffff_ffff) * 512)
    );
    record[4..].fill(0xff);
    record[..2].fill(0);
    assert!(matches!(
        core.resolve_chunk_read(&record, 0x20, 0, 0, 1).unwrap(),
        BlockPlan::Hole { size: 1 }
    ));
    assert!(matches!(
        core.resolve_chunk_read(&record, 0x60, 0, 0, 1).unwrap(),
        BlockPlan::Direct { .. }
    ));
    record[..2].fill(0xff);
    assert!(matches!(
        core.resolve_chunk_read(&record, 0x60, 0, 0, 1).unwrap(),
        BlockPlan::Hole { size: 1 }
    ));
    assert!(matches!(
        core.resolve_chunk_read(&u32::MAX.to_le_bytes(), 0x40, 0, 0, 1)
            .unwrap(),
        BlockPlan::Hole { .. }
    ));
    assert!(
        core.resolve_chunk_read(&record[..7], 0x60, 0, 0, 1)
            .is_err()
    );
    assert_eq!(
        core.resolve_device(0, (high + 102) * 512, 17).unwrap(),
        (1, 1024)
    );
    assert_eq!(core.resolve_device(0, 20 * 512, 17).unwrap(), (2, 0)); // Table order differs from address order.
    for (at, size) in [(20 * 512 - 1, 2), (24 * 512 - 1, 2)] {
        assert!(core.resolve_device(0, at, size).is_err());
    }
    assert!(core.resolve_device(1, u64::MAX, 2).is_err());
    assert!(DeviceTable::parse(&data[1280..1535], 512, 13, true).is_err());
    assert!(DeviceTable::parse(&data[1280..1536], 1 << 24, 13, true).is_err());
    // Single-device images mask off the entire device field, including high address noise.
    data[1104] = 4;
    let core = EroFSCore::new(&data[1024..1152]).unwrap();
    record.fill(0xff);
    record[4..].copy_from_slice(&0x8000_0000u32.to_le_bytes());
    assert!(
        matches!(core.resolve_chunk_read(&record, 0x20, 0, 0, 1).unwrap(),
        BlockPlan::Direct { device_id: 0, offset, .. } if offset == 0x8000_0000u64 * 512)
    );
}
