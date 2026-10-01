use super::*;
use crate::{
    backend::SliceImage,
    tests::{Source, ready},
    types::MAGIC_NUMBER,
};
use core::sync::atomic::{AtomicBool, AtomicUsize};
#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
use {crate::tests::Read, core::sync::atomic::Ordering::Relaxed};

// Independent native liblz4, liblzma (MicroLZMA), zlib (raw), and libzstd
// samples: 700 'A' bytes followed by 325 'B' bytes. No encoder is needed at runtime.
fn samples() -> Vec<(u8, Encoding, &'static [u8])> {
    vec![
        (
            0,
            Encoding::Lz4,
            &[
                31, 65, 1, 0, 255, 255, 170, 31, 66, 1, 0, 255, 45, 80, 66, 66, 66, 66, 66,
            ],
        ),
        (
            1,
            Encoding::Lzma(32768),
            &[
                162, 32, 239, 251, 191, 254, 139, 14, 32, 28, 116, 69, 67, 204, 0,
            ],
        ),
        (
            2,
            Encoding::Deflate,
            &[115, 116, 28, 5, 163, 96, 104, 2, 167, 81, 64, 57, 0, 0],
        ),
        (
            3,
            Encoding::Zstd(1 << 20),
            &[
                40, 181, 47, 253, 100, 1, 3, 109, 0, 0, 24, 65, 65, 66, 2, 0, 65, 64, 21, 183, 42,
                128, 5, 33, 217, 163, 44,
            ],
        ),
    ]
}

fn enabled_samples() -> impl Iterator<Item = (u8, Encoding, &'static [u8])> {
    samples()
        .into_iter()
        .filter(|(_, encoding, _)| encoding.require_enabled().is_ok())
}

fn expected() -> Vec<u8> {
    [vec![b'A'; 700], vec![b'B'; 325]].concat()
}

fn padded(payload: &[u8]) -> Vec<u8> {
    let mut block = vec![0; 512];
    block[512 - payload.len()..].copy_from_slice(payload);
    block
}

// A compact inode with Full compression indexes: [0, 1025) is compressed,
// [1025, 1500) is shifted PLAIN. The last logical block crosses both extents.
fn image(algorithm: u8, payload: &[u8]) -> Vec<u8> {
    let mut data = vec![0; 5120];
    data[1024..1028].copy_from_slice(&MAGIC_NUMBER.to_le_bytes());
    data[1036] = 9;
    data[1064..1068].copy_from_slice(&4u32.to_le_bytes());
    data[1104] = 3; // Compression configuration and leading zero padding.
    data[1108..1110].copy_from_slice(&(1u16 << algorithm).to_le_bytes());
    data[1152] = if algorithm < 2 { 14 } else { 6 };
    match algorithm {
        1 => data[1154..1158].copy_from_slice(&32768u32.to_le_bytes()),
        2 => data[1154] = 15,
        3 => data[1155] = 10,
        _ => {}
    }
    data[2080] = 2; // CompressedFull, compact inode, nid 1.
    data[2084..2086].copy_from_slice(&0o100644u16.to_le_bytes());
    data[2088..2092].copy_from_slice(&1500u32.to_le_bytes());
    data[2118] = algorithm;
    for (at, kind, within, block) in [(2128, 1u16, 0u16, 8u32), (2136, 2, 0, 1), (2144, 0, 1, 9)] {
        data[at..at + 2].copy_from_slice(&kind.to_le_bytes());
        data[at + 2..at + 4].copy_from_slice(&within.to_le_bytes());
        data[at + 4..at + 8].copy_from_slice(&block.to_le_bytes());
    }
    data[4096..4608].copy_from_slice(&padded(payload));
    data[4608..5083].fill(b'C');
    data
}

fn metadata(data: &[u8]) -> (EroFSCore, Inode) {
    let core = EroFSCore::new(&data[1024..1152]).unwrap();
    let inode = core.parse_inode(&data[2080..2144], 1).unwrap();
    (core, inode)
}

fn plan(data: &[u8], offset: u64) -> Result<BlockPlan> {
    let (core, inode) = metadata(data);
    let mut plan = core.plan_inode_read(&inode, offset)?;
    while let BlockPlan::CompressionMetadata {
        offset,
        size,
        reader,
    } = plan
    {
        let bytes = data
            .get(offset as usize..offset as usize + size)
            .ok_or_else(|| Error::OutOfBounds("test metadata truncated".into()))?;
        plan = reader.resume(&core, offset, bytes)?;
    }
    Ok(plan)
}

#[test]
fn plain_holes_and_fragments_do_not_require_codecs() {
    let wanted: Vec<u8> = (0..512).map(|n| n as u8).collect();
    for compact in [false, true] {
        let mut data = image(0, &[]);
        data[2080] = if compact { 6 } else { 2 };
        data[2088..2092].copy_from_slice(&512u32.to_le_bytes());
        data[2120..2152].fill(0);
        data[if compact { 2124 } else { 2132 }] = if compact { 7 } else { 8 };
        data[4096..4608].copy_from_slice(&wanted);
        check_image(&data, &wanted);
        data[2116] = 0x10; // Interlaced PLAIN uses the same feature-independent mapping.
        check_image(&data, &wanted);
    }
    for bits in 0..4 {
        let mut data = image(0, &[]);
        let record_size = 4usize << bits;
        data[2088..2092].copy_from_slice(&512u32.to_le_bytes());
        data[2112..2240].fill(0);
        data[2112] = 1;
        data[2116] = 1 | (bits << 1);
        let mut at = 2120usize.next_multiple_of(record_size);
        if bits == 0 {
            data[at..at + 8].copy_from_slice(&4096u64.to_le_bytes());
            at += 8;
        } else {
            data[at + 4..at + 8].copy_from_slice(&4096u32.to_le_bytes());
        }
        check_image(&data, &[0; 512]);
        data[at..at + 4].copy_from_slice(&512u32.to_le_bytes());
        data[4096..4608].copy_from_slice(&wanted);
        check_image(&data, &wanted);
    }
    let mut data = image(0, &[]);
    data[1104] |= 0x20;
    data[1120..1128].copy_from_slice(&16u64.to_le_bytes());
    data[2088..2092].copy_from_slice(&17u32.to_le_bytes());
    data[2112..2120].copy_from_slice(&((1u64 << 63) | 7).to_le_bytes());
    data[2564..2566].copy_from_slice(&0o100644u16.to_le_bytes());
    data[2568..2572].copy_from_slice(&512u32.to_le_bytes());
    data[2576] = 8; // Flat packed inode.
    data[4096..4608].copy_from_slice(&wanted);
    check_image(&data, &wanted[7..24]);
    for size in [6u32, 23] {
        data[2568..2572].copy_from_slice(&size.to_le_bytes());
        let source = Source {
            data: SliceImage::new(&data),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let fs = crate::EroFS::new(&source).unwrap();
        let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
        let inode = fs.get_inode(1).unwrap();
        assert!(matches!(
            fs.get_inode_data(&inode, 0),
            Err(Error::CorruptedData(_))
        ));
        assert!(matches!(
            ready(afs.read_inode_data(&inode, 0)),
            Err(Error::CorruptedData(_))
        ));
    }
}

#[test]
fn compact_pack_boundaries_and_entry_decoding() {
    let (core, mut inode) = metadata(&image(0, &[]));
    inode.data = InodeData::CompressedCompact;
    inode.data_size = 24 * 512;
    let mut layout = Index::new(&core, &inode, 0, &[0, 0, 0, 0, 1, 0, 0, 0]).unwrap();
    for (index, expected) in [
        (0, (8, 0, 8)),
        (5, (24, 1, 8)),
        (6, (32, 0, 32)),
        (21, (32, 15, 32)),
        (22, (64, 0, 8)),
        (23, (64, 1, 8)),
    ] {
        assert_eq!(layout.pack(index).unwrap(), expected);
    }
    assert!(matches!(layout.pack(24), Err(Error::CorruptedData(_))));
    // 16 packed 14-bit entries: HEAD, NONHEAD distances 1..13, HEAD(+17),
    // final NONHEAD with forward distance 73; preceding physical block = 10.
    let pack = [
        0, 80, 0, 40, 0, 14, 128, 4, 96, 1, 104, 0, 30, 128, 8, 96, 2, 168, 0, 46, 128, 12, 96, 3,
        24, 1, 37, 129, 10, 0, 0, 0,
    ];
    let Some(Entry::Head(head)) = layout.entry(20, 32, &pack).unwrap() else {
        panic!("HEAD expected")
    };
    assert_eq!((head.start, head.block), (20 * 512 + 17, 12));
    assert!(matches!(
        layout.entry(21, 32, &pack).unwrap(),
        Some(Entry::NonHead(NonHead {
            back: 1,
            blocks: None
        }))
    ));
    assert!(layout.entry(20, 32, &pack[..31]).unwrap().is_none());

    // With block-count indexes the base is the first physical block, not its
    // predecessor. Change the first NONHEAD from distance 1 to CBLKCNT | 1.
    let mut pack = pack;
    pack[3] |= 2;
    layout.big1 = true;
    layout.big2 = true;
    let Some(Entry::Head(head)) = layout.entry(20, 32, &pack).unwrap() else {
        panic!("HEAD expected")
    };
    assert_eq!((head.start, head.block), (20 * 512 + 17, 11));
}

#[test]
fn compact_last_nonhead_and_wrapping_predictor() {
    let (core, mut inode) = metadata(&image(0, &[]));
    inode.data = InodeData::CompressedCompact;
    let layout = Index::new(&core, &inode, 0, &[0; 8]).unwrap();
    let pack = [0, 0x10, 7, 0x20, 255, 255, 255, 255];
    assert!(matches!(
        layout.entry(0, 8, &pack).unwrap(),
        Some(Entry::Head(Head { block: 0, .. }))
    ));
    assert!(matches!(
        layout.entry(1, 8, &pack).unwrap(),
        Some(Entry::NonHead(NonHead {
            back: 1,
            blocks: None
        }))
    ));
    // Reconstructed 2048 is a distance, NOT an encoded CBLKCNT marker.
    let pack = [255, 0x27, 7, 0x20, 0, 0, 0, 0];
    assert!(matches!(
        layout.entry(1, 8, &pack).unwrap(),
        Some(Entry::NonHead(NonHead {
            back: 2048,
            blocks: None
        }))
    ));
}

#[test]
fn block_count_markers_are_not_lookback_distances() {
    let (core, mut inode) = metadata(&image(0, &[]));
    inode.data = InodeData::CompressedCompact;
    let mut layout = Index::new(&core, &inode, 0, &[0; 8]).unwrap();
    assert!(matches!(
        layout.nonhead(CBLKCNT | 1),
        Err(Error::CorruptedData(_))
    ));
    layout.big1 = true;
    layout.big2 = true;
    assert!(matches!(
        layout.nonhead(CBLKCNT | 1),
        Ok(NonHead {
            back: 1,
            blocks: Some(1)
        })
    ));
    for bad in [0, 1, CBLKCNT] {
        assert!(matches!(layout.nonhead(bad), Err(Error::CorruptedData(_))));
    }
    assert!(matches!(
        layout.nonhead(CBLKCNT | 2),
        Ok(NonHead {
            back: 1,
            blocks: Some(2)
        })
    ));
    assert!(matches!(
        layout.nonhead(2),
        Ok(NonHead {
            back: 2,
            blocks: None
        })
    ));
}

fn feed(plan: BlockPlan, core: &EroFSCore, at: u64, data: &[u8]) -> Result<BlockPlan> {
    let BlockPlan::CompressionMetadata {
        offset,
        size,
        reader,
    } = plan
    else {
        panic!("metadata request expected")
    };
    assert_eq!((offset, size), (at, data.len()));
    reader.resume(core, at, data)
}

#[test]
fn configuration_records_follow_ext_slots_and_four_byte_alignment() {
    let (mut core, inode) = metadata(&image(0, &[]));
    core.super_block.ext_slots = 3;
    core.super_block.compr_algs = 15;
    let mut plan = core.plan_inode_read(&inode, 0).unwrap();
    let mut lzma = [0; 14];
    lzma[..4].copy_from_slice(&32768u32.to_le_bytes());
    // An extended 15-byte LZ4 config forces alignment padding before LZMA.
    for (at, config) in [
        (1200, &[0; 15][..]),
        (1220, &lzma[..]),
        (1236, &[15, 0, 0, 0, 0, 0][..]),
        (1244, &[0, 10, 0, 0, 0, 0][..]),
    ] {
        plan = feed(plan, &core, at, &(config.len() as u16).to_le_bytes()).unwrap();
        plan = feed(plan, &core, at + 2, config).unwrap();
    }
    let BlockPlan::CompressionMetadata {
        offset,
        size,
        reader,
    } = plan
    else {
        panic!("header expected")
    };
    assert_eq!((offset, size), (2112, 8));
    assert_eq!(
        (reader.lzma_dict_size, reader.zstd_window_size),
        (32768, 1 << 20)
    );
}

#[test]
fn invalid_configuration_and_unsupported_formats_are_rejected() {
    for algorithm in 0..4 {
        for size in [0, 1, 5] {
            let mut data = image(algorithm, &[]);
            data[1152] = size;
            assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
        }
    }
    for dictionary in [0u32, 4095, MAX_LZMA_DICT_SIZE + 1] {
        let mut data = image(1, &[]);
        data[1154..1158].copy_from_slice(&dictionary.to_le_bytes());
        assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
    }
    for (algorithm, at, value) in [(1, 1158, 1), (3, 1154, 1)] {
        let mut data = image(algorithm, &[]);
        data[at] = value;
        assert!(matches!(plan(&data, 0), Err(Error::NotSupported(_))));
    }
    for (algorithm, at, value) in [(2, 1154, 7), (2, 1154, 16), (3, 1155, 11)] {
        let mut data = image(algorithm, &[]);
        data[at] = value;
        assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
    }
}

#[test]
fn head2_bitmap_and_codec_feature_selection() {
    for (algorithm, encoding, payload) in samples() {
        let mut data = image(algorithm, payload);
        let BlockPlan::Encoded(extent) = plan(&data, 0).unwrap() else {
            panic!("encoded extent expected")
        };
        let decoded = extent.decode(&padded(payload), usize::MAX);
        if encoding.require_enabled().is_ok() {
            assert_eq!(decoded.unwrap(), expected());
        } else {
            assert!(matches!(decoded, Err(Error::NotSupported(_))));
        }
        data[2118] <<= 4;
        data[2128] = 3;
        assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
        data[1104] |= 8;
        assert!(matches!(plan(&data, 0), Ok(BlockPlan::Encoded(_))));
        data[1108] = 0;
        assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
    }
}

#[test]
fn full_index_walk_checks_lookback_and_block_counts() {
    let algorithm = samples()[0].0;
    for (at, bytes) in [
        (2128, &[2, 0][..]),
        (2140, &[0, 0][..]),
        (2140, &[2, 0][..]),
    ] {
        let mut data = image(algorithm, &[]);
        data[at..at + bytes.len()].copy_from_slice(bytes);
        assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
    }
    let mut data = image(algorithm, &[]);
    data[2116] = 2;
    assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
    data[2140..2142].copy_from_slice(&(CBLKCNT | 1).to_le_bytes());
    assert!(matches!(plan(&data, 0), Ok(BlockPlan::Encoded(_))));
    data[2140..2142].copy_from_slice(&(CBLKCNT | 2).to_le_bytes());
    assert!(matches!(
        plan(&data, 0),
        Ok(BlockPlan::Encoded(EncodedExtent { size: 1024, .. }))
    ));
}

#[test]
fn decoded_extent_limit_is_checked_before_decoding() {
    let algorithm = samples()[0].0;
    for length in [MAX_DECODED_SIZE, MAX_DECODED_SIZE + 1] {
        let count = length.div_ceil(512) as usize;
        let mut data = image(algorithm, &[]);
        data.resize(2128 + count * 8, 0);
        data[2088..2092].copy_from_slice(&(length as u32).to_le_bytes());
        for index in 1..count {
            let at = 2128 + index * 8;
            data[at..at + 8].fill(0);
            data[at] = 2;
            data[at + 4..at + 6].copy_from_slice(&(index.min(2047) as u16).to_le_bytes());
        }
        let result = plan(&data, 0);
        if length == MAX_DECODED_SIZE {
            assert!(
                matches!(result, Ok(BlockPlan::Encoded(EncodedExtent { decoded_size, .. })) if decoded_size as u64 == length)
            );
        } else {
            assert!(matches!(result, Err(Error::NotSupported(_))));
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
fn codec_samples_require_complete_input_and_exact_output() {
    let wanted = expected();
    for (algorithm, encoding, payload) in enabled_samples() {
        let mut extent = EncodedExtent {
            offset: 0,
            size: 512,
            decoded_size: 1025,
            skip: 0,
            encoding,
            partial: false,
            zero_padding: true,
        };
        let input = padded(payload);
        assert_eq!(
            extent.decode(&input, usize::MAX).unwrap(),
            wanted,
            "algorithm {algorithm}"
        );
        extent.skip = 701;
        assert_eq!(extent.decode(&input, usize::MAX).unwrap(), wanted[701..]);
        assert_eq!(extent.decode(&input, 13).unwrap(), wanted[701..714]);
        extent.skip = 0;
        for size in [1024, 1026] {
            extent.decoded_size = size;
            assert!(matches!(
                extent.decode(&input, usize::MAX),
                Err(Error::CorruptedData(_))
            ));
        }
        extent.decoded_size = 1025;
        for bad in [
            vec![0; 512],
            padded(&payload[..payload.len() - 1]),
            padded(&[payload, &[0]].concat()),
            input[..511].to_vec(),
        ] {
            assert!(
                matches!(extent.decode(&bad, 13), Err(Error::CorruptedData(_))),
                "algorithm {algorithm}"
            );
        }
    }
}

#[cfg(feature = "lzma")]
#[test]
fn microlzma_properties_are_validated_before_decoder_construction() {
    for properties in [8, 44, 224, 225, 254] {
        let input = [!properties, 0, 0, 0, 0];
        let error = decode_microlzma(&input, &mut [0; 1], 32768, false).unwrap_err();
        assert!(
            matches!(error, Error::CorruptedData(ref message) if message == "invalid MicroLZMA properties")
        );
    }
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_checks_checksum_window_descriptor_and_content_size() {
    let (_, _, sample) = samples().into_iter().find(|s| s.0 == 3).unwrap();
    let mut output = vec![0; 1025];
    assert!(decode_zstd(sample, &mut output, 1024, false).is_err());
    for (at, xor) in [(4, 8), (5, 1), (sample.len() - 1, 1)] {
        let mut bad = sample.to_vec();
        bad[at] ^= xor;
        assert!(matches!(
            decode_zstd(&bad, &mut output, 1 << 20, false),
            Err(Error::CorruptedData(_))
        ));
    }
    assert!(decode_zstd(&[sample, sample].concat(), &mut output, 1 << 20, false).is_err());
}

#[test]
fn plain_extent_mapping_and_interlaced_crop() {
    let data = image(samples()[0].0, &[]);
    assert!(matches!(
        plan(&data, 1030).unwrap(),
        BlockPlan::Direct {
            offset: 4613,
            size: 470
        }
    ));
    let extent = EncodedExtent {
        offset: 0,
        size: 8,
        decoded_size: 6,
        skip: 2,
        encoding: Encoding::Interlaced(6),
        partial: false,
        zero_padding: true,
    };
    assert_eq!(extent.decode(b"abcdefgh", usize::MAX).unwrap(), b"abcd");
    assert_eq!(extent.decode(b"abcdefgh", 2).unwrap(), b"ab");
    for (algorithm, _, payload) in enabled_samples() {
        let mut data = image(algorithm, payload);
        data[2088..2092].copy_from_slice(&1325u32.to_le_bytes());
        data[1104] |= 0x10;
        data[2116] = 0x18; // Interlaced PLAIN inline tail, with one leading byte.
        data[2114..2116].copy_from_slice(&301u16.to_le_bytes());
        data[2152] = b'!';
        data[2153..2453].fill(b'C');
        check_image(&data, &[expected(), vec![b'C'; 300]].concat());
        data[2114..2116].copy_from_slice(&300u16.to_le_bytes());
        assert!(plan(&data, 1025).is_err());
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[test]
fn partial_references_stop_inside_literals_and_matches() {
    for (algorithm, encoding, payload) in enabled_samples() {
        let mut extent = EncodedExtent {
            offset: 0,
            size: 512,
            decoded_size: 1,
            skip: 0,
            encoding,
            partial: true,
            zero_padding: true,
        };
        let input = padded(payload);
        let wanted = expected();
        for size in [1, 2, 699, 700, 701, 1020, 1024, 1025] {
            extent.decoded_size = size;
            assert_eq!(
                extent.decode(&input, usize::MAX).unwrap(),
                wanted[..size],
                "codec {algorithm}, prefix {size}"
            );
        }
        extent.decoded_size = 1026;
        assert!(extent.decode(&input, usize::MAX).is_err());
    }
}

fn check_image(data: &[u8], wanted: &[u8]) {
    let source = Source {
        data: SliceImage::new(data),
        reads: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    };
    let fs = crate::sync::EroFS::new(&source).unwrap();
    let inode = fs.get_inode(1).unwrap();
    let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
    let ainode = ready(afs.get_inode(1)).unwrap();
    for offset in (0..wanted.len()).step_by(113) {
        let size = (wanted.len() - offset).min(512);
        assert_eq!(
            &*fs.get_inode_block(&inode, offset as u64).unwrap(),
            &wanted[offset..offset + size]
        );
        assert_eq!(
            ready(afs.read_inode_block(&ainode, offset as u64)).unwrap(),
            wanted[offset..offset + size]
        );
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[test]
fn multiple_blocks_inline_tails_and_nondefault_clusters() {
    for (algorithm, _, payload) in enabled_samples() {
        for compact in [false, true] {
            let mut data = image(algorithm, payload);
            data.resize(6144, 0);
            data[2080] = if compact { 6 } else { 2 };
            data[2088..2092].copy_from_slice(&1025u32.to_le_bytes());
            data[2116] = if compact { 6 } else { 2 };
            data[2120..2152].fill(0);
            if compact {
                data[2120..2122].copy_from_slice(&0x1000u16.to_le_bytes());
                data[2122..2124].copy_from_slice(&(0x2000 | CBLKCNT | 2).to_le_bytes());
                data[2124..2128].copy_from_slice(&8u32.to_le_bytes());
                data[2128..2130].copy_from_slice(&0x2002u16.to_le_bytes());
            } else {
                data[2128] = 1;
                data[2132] = 8;
                data[2136] = 2;
                data[2140..2142].copy_from_slice(&(CBLKCNT | 2).to_le_bytes());
                data[2144] = 2;
                data[2148] = 2;
            }
            data[4096..5120].fill(0);
            data[5120 - payload.len()..5120].copy_from_slice(payload);
            check_image(&data, &expected());

            data[1104] |= 0x10;
            data[2116] |= 8;
            data[2114..2116].copy_from_slice(&(payload.len() as u16).to_le_bytes());
            let at = if compact { 2136 } else { 2152 };
            data[at..at + payload.len()].copy_from_slice(payload);
            check_image(&data, &expected());
        }
        let mut data = image(algorithm, payload);
        data[2088..2092].copy_from_slice(&1025u32.to_le_bytes());
        data[2119] = 2; // 2 KiB logical cluster in a 512-byte filesystem.
        data.resize(8192, 0);
        data[4096..6144].fill(0);
        data[6144 - payload.len()..6144].copy_from_slice(payload);
        check_image(&data, &expected());
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[test]
fn extent_record_sizes_holes_and_partial_references() {
    for (algorithm, _, payload) in enabled_samples() {
        for bits in 0..4 {
            let mut data = image(algorithm, payload);
            let record_size = 4usize << bits;
            data[2088..2092].copy_from_slice(&1536u32.to_le_bytes());
            data[2112..2240].fill(0);
            data[2116] = 1 | (bits << 1);
            if bits >= 2 {
                data[2112] = 3;
            }
            let mut at = 2120usize.next_multiple_of(record_size);
            if bits == 0 {
                data[at..at + 8].copy_from_slice(&4096u64.to_le_bytes());
                at += 8;
            }
            // Two references to prefixes of the same payload, followed by a hole.
            // Four-byte records use consecutive physical locations instead.
            for index in 0..3 {
                let pos = at + index * record_size;
                let plen = if index == 2 {
                    0
                } else {
                    512 | (1 << 27) | ((u32::from(algorithm) + 1) << 28)
                };
                data[pos..pos + 4].copy_from_slice(&plen.to_le_bytes());
                if bits >= 1 {
                    data[pos + 4..pos + 8].copy_from_slice(&4096u32.to_le_bytes());
                }
                if bits >= 2 {
                    data[pos + 12..pos + 16].copy_from_slice(&(index as u32 * 512).to_le_bytes());
                }
            }
            data[4608..5120].copy_from_slice(&padded(payload));
            check_image(&data, &[vec![b'A'; 1024], vec![0; 512]].concat());
        }
    }
}

#[cfg(feature = "lz4")]
#[test]
fn legacy_lz4_uses_trailing_padding() {
    let (_, encoding, payload) = samples().into_iter().find(|s| s.0 == 0).unwrap();
    let mut input = vec![0; 512];
    input[..payload.len()].copy_from_slice(payload);
    let extent = EncodedExtent {
        offset: 0,
        size: 512,
        decoded_size: 1025,
        skip: 0,
        encoding,
        partial: false,
        zero_padding: false,
    };
    assert_eq!(extent.decode(&input, usize::MAX).unwrap(), expected());
    for bad in [&[0, 0, 0][..], &[15, 1, 0, 255][..], &[0xf0, 255][..]] {
        assert!(decode_lz4_prefix(bad, &mut [0; 512]).is_err());
    }
    let mut data = image(0, payload);
    data[1104] &= !1;
    data[4096..4608].copy_from_slice(&input);
    check_image(&data, &[expected(), vec![b'C'; 475]].concat());
}

#[test]
fn advanced_metadata_limits_and_invalid_ranges() {
    let (algorithm, _, payload) = samples().remove(0);
    let mut data = image(algorithm, payload);
    data[2118] = 15; // Must not wrap (algorithm + 1) into the PLAIN format.
    assert!(plan(&data, 0).is_err());
    data[2118] = algorithm;
    data[2116] = 8;
    data[2114..2116].copy_from_slice(&500u16.to_le_bytes());
    assert!(plan(&data, 1025).is_err()); // Missing superblock feature.
    data[1104] |= 0x10;
    assert!(plan(&data, 1025).is_err()); // Crosses the metadata block boundary.
    data[2116] |= 0x20;
    assert!(plan(&data, 1025).is_err()); // Inline and fragment are exclusive.

    data[2112..2160].fill(0);
    data[2112] = 1;
    data[2116] = 5; // One 16-byte extent record.
    for plen in [0x20_0000, MAX_ENCODED_SIZE as u32 + 1] {
        data[2128..2132]
            .copy_from_slice(&(plen | ((u32::from(algorithm) + 1) << 28)).to_le_bytes());
        assert!(plan(&data, 0).is_err());
    }
    data[2128..2132].copy_from_slice(&(512 | ((u32::from(algorithm) + 1) << 28)).to_le_bytes());
    data[2132..2140].fill(255);
    assert!(matches!(plan(&data, 0), Err(Error::Overflow(_))));
    data[2112] = 0;
    assert!(plan(&data, 0).is_err());
}

#[test]
fn wide_extent_addresses_and_large_holes() {
    for (algorithm, _, payload) in samples() {
        let mut data = image(algorithm, payload);
        data[2080..2272].fill(0);
        data[2080] = 3; // Extended inode, Full indexes.
        data[2084..2086].copy_from_slice(&0o100644u16.to_le_bytes());
        data[2088..2096].copy_from_slice(&((1u64 << 32) + 1025).to_le_bytes());
        data[2144] = 2; // Two 32-byte records; the first is a 4 GiB hole.
        data[2148] = 7;
        data[2208..2212].copy_from_slice(&(512 | ((u32::from(algorithm) + 1) << 28)).to_le_bytes());
        data[2212..2220].copy_from_slice(&((1u64 << 40) + 4096).to_le_bytes());
        data[2220..2228].copy_from_slice(&(1u64 << 32).to_le_bytes());
        assert!(matches!(
            plan(&data, 0).unwrap(),
            BlockPlan::Hole { size: 512 }
        ));
        assert!(matches!(
            plan(&data, (1 << 32) - 1).unwrap(),
            BlockPlan::Hole { size: 1 }
        ));
        let BlockPlan::Encoded(extent) = plan(&data, (1 << 32) + 700).unwrap() else {
            panic!("encoded extent expected")
        };
        assert_eq!((extent.offset, extent.skip), ((1 << 40) + 4096, 700));
        if extent.encoding.require_enabled().is_ok() {
            assert_eq!(
                extent.decode(&padded(payload), usize::MAX).unwrap(),
                expected()[700..]
            );
        }
    }
}

#[test]
fn compact_nondefault_cluster_pack_regions_use_filesystem_blocks() {
    let (core, mut inode) = metadata(&image(0, &[]));
    inode.data = InodeData::CompressedCompact;
    inode.data_size = 24 * 1024;
    let layout = Index::new(&core, &inode, 0, &[0, 0, 0, 0, 1, 0, 0, 1]).unwrap();
    assert_eq!(layout.count, 24);
    assert_eq!(layout.pack(23).unwrap(), (64, 1, 32));
    assert_eq!(layout.end, 96);
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[test]
fn packed_fragments_are_bounded_and_cannot_recurse() {
    for (algorithm, _, payload) in enabled_samples() {
        let mut data = image(algorithm, payload);
        data.resize(6144, 0);
        data[1104] |= 0x20;
        data[1120..1128].copy_from_slice(&16u64.to_le_bytes());
        // The packed inode is itself compressed. Reference an interior range.
        let packed = data[2080..2152].to_vec();
        data[2560..2632].copy_from_slice(&packed);
        data[2088..2092].copy_from_slice(&325u32.to_le_bytes());
        data[2112..2120].copy_from_slice(&((1u64 << 63) | 700).to_le_bytes());
        check_image(&data, &vec![b'B'; 325]);
        let mut short_extents = data.clone();
        short_extents[2088..2092].copy_from_slice(&837u32.to_le_bytes());
        short_extents[2112..2144].fill(0);
        short_extents[2116] = 0x21; // Four-byte extents: a hole, then a fragment.
        short_extents[2120..2128].copy_from_slice(&u64::MAX.to_le_bytes());
        short_extents[2132..2136].copy_from_slice(&700u32.to_le_bytes());
        check_image(&short_extents, &[vec![0; 512], vec![b'B'; 325]].concat());

        // A Full-index fragment can use HEAD2 and a zero block-count marker
        // even without BIG_PCLUSTER_2 (native mkfs uses this for non-LZ4).
        data[2088..2092].copy_from_slice(&1025u32.to_le_bytes());
        data[2112..2120].fill(0);
        data[2116] = 0x22;
        data[2128] = 3;
        data[2132..2136].fill(0);
        data[2140..2142].copy_from_slice(&CBLKCNT.to_le_bytes());
        data[2144] = 2;
        data[2146] = 0;
        data[2148..2152].copy_from_slice(&2u32.to_le_bytes());
        check_image(&data, &expected());
        data[2088..2092].copy_from_slice(&325u32.to_le_bytes());
        data[2112..2120].copy_from_slice(&((1u64 << 63) | 1499).to_le_bytes());
        let fs = crate::sync::EroFS::new(SliceImage::new(&data)).unwrap();
        assert!(fs.get_inode_data(&fs.get_inode(1).unwrap(), 0).is_err());
        data[2112..2120].copy_from_slice(&(1u64 << 63).to_le_bytes());
        data[2592..2600].copy_from_slice(&(1u64 << 63).to_le_bytes());
        let fs = crate::sync::EroFS::new(SliceImage::new(&data)).unwrap();
        assert!(fs.get_inode_data(&fs.get_inode(1).unwrap(), 0).is_err());
    }
}

#[cfg(any(
    feature = "lz4",
    feature = "lzma",
    feature = "deflate",
    feature = "zstd"
))]
#[test]
fn sync_async_block_assembly_cache_and_retry_contracts() {
    let wanted = [expected(), vec![b'C'; 475]].concat();
    for (algorithm, _, payload) in enabled_samples() {
        for fragment in [false, true] {
            let mut data = image(algorithm, payload);
            if fragment {
                data[1104] |= 0x20;
                data[1120..1128].copy_from_slice(&16u64.to_le_bytes());
                let packed = data[2080..2152].to_vec();
                data[2560..2632].copy_from_slice(&packed);
                data[2112..2120].copy_from_slice(&(1u64 << 63).to_le_bytes());
            }
            let source = Source {
                data: SliceImage::new(&data),
                reads: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
            };
            let fs = crate::sync::EroFS::new(&source).unwrap();
            let inode = fs.get_inode(1).unwrap();
            let afs = ready(crate::r#async::EroFS::new(&source)).unwrap();
            let ainode = ready(afs.get_inode(1)).unwrap();
            for offset in [0, 512, 1024] {
                let expected = &wanted[offset..(offset + 512).min(wanted.len())];
                assert_eq!(
                    &*fs.get_inode_block(&inode, offset as u64).unwrap(),
                    expected
                );
                assert_eq!(
                    ready(afs.read_inode_block(&ainode, offset as u64)).unwrap(),
                    expected
                );
            }
            let mut file = fs.open_inode_file(inode).unwrap();
            let mut afile = afs.open_inode_file(ainode).unwrap();
            source.fail.store(true, Relaxed);
            let reads = source.reads.load(Relaxed);
            assert_eq!(file.read(&mut []).unwrap(), 0);
            assert_eq!(ready(afile.read(&mut [])).unwrap(), 0);
            assert_eq!(source.reads.load(Relaxed), reads);
            let mut buf = [0; 7];
            assert!(file.read(&mut buf).is_err());
            assert!(ready(afile.read(&mut buf)).is_err());
            source.fail.store(false, Relaxed);
            assert_eq!(file.read(&mut buf).unwrap(), 7);
            assert_eq!(buf, wanted[..7]);
            assert_eq!(ready(afile.read(&mut buf)).unwrap(), 7);
            assert_eq!(buf, wanted[..7]);
            source.fail.store(true, Relaxed);
            let reads = source.reads.load(Relaxed);
            let mut buf = [0; 1018];
            assert_eq!(file.read(&mut buf).unwrap(), 1018);
            assert_eq!(buf, wanted[7..1025]);
            assert_eq!(ready(afile.read(&mut buf)).unwrap(), 1018);
            assert_eq!(buf, wanted[7..1025]);
            assert_eq!(source.reads.load(Relaxed), reads);
            assert!(file.read(&mut buf).is_err());
            assert!(ready(afile.read(&mut buf)).is_err());
            source.fail.store(false, Relaxed);
            assert_eq!(file.read(&mut buf).unwrap(), 475);
            assert_eq!(buf[..475], wanted[1025..]);
            assert_eq!(ready(afile.read(&mut buf)).unwrap(), 475);
            assert_eq!(buf[..475], wanted[1025..]);
            source.fail.store(true, Relaxed);
            let reads = source.reads.load(Relaxed);
            assert_eq!(file.read(&mut buf).unwrap(), 0);
            assert_eq!(ready(afile.read(&mut buf)).unwrap(), 0);
            assert_eq!(source.reads.load(Relaxed), reads);
        }
    }
}
