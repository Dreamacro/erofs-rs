use super::*;
#[cfg(not(feature = "std"))]
use crate::sync::file::Read;
use crate::{
    backend::{AsyncImage, Image, SliceImage},
    types::MAGIC_NUMBER,
};
use core::{
    future::Future,
    ops::RangeBounds,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    task::{Context, Poll, Waker},
};
#[cfg(feature = "std")]
use std::io::Read;

// Independent native liblz4, liblzma (MicroLZMA), zlib (raw), and libzstd
// samples: 700 'A' bytes followed by 325 'B' bytes. No encoder is needed at runtime.
fn samples() -> Vec<(u8, Encoding, &'static [u8])> {
    vec![
        #[cfg(feature = "lz4")]
        (
            0,
            Encoding::Lz4,
            &[
                31, 65, 1, 0, 255, 255, 170, 31, 66, 1, 0, 255, 45, 80, 66, 66, 66, 66, 66,
            ],
        ),
        #[cfg(feature = "lzma")]
        (
            1,
            Encoding::Lzma(32768),
            &[
                162, 32, 239, 251, 191, 254, 139, 14, 32, 28, 116, 69, 67, 204, 0,
            ],
        ),
        #[cfg(feature = "deflate")]
        (
            2,
            Encoding::Deflate,
            &[115, 116, 28, 5, 163, 96, 104, 2, 167, 81, 64, 57, 0, 0],
        ),
        #[cfg(feature = "zstd")]
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
    let inode = core.parse_inode(&data[2080..2112], 1).unwrap();
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
        Some(Entry::NonHead(NonHead { back: 1, blocks: 0 }))
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
        Some(Entry::NonHead(NonHead { back: 1, blocks: 0 }))
    ));
    // Reconstructed 2048 is a distance, NOT an encoded CBLKCNT marker.
    let pack = [255, 0x27, 7, 0x20, 0, 0, 0, 0];
    assert!(matches!(
        layout.entry(1, 8, &pack).unwrap(),
        Some(Entry::NonHead(NonHead {
            back: 2048,
            blocks: 0
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
        Ok(NonHead { back: 1, blocks: 1 })
    ));
    for bad in [0, 1, CBLKCNT] {
        assert!(matches!(layout.nonhead(bad), Err(Error::CorruptedData(_))));
    }
    assert!(matches!(
        layout.nonhead(CBLKCNT | 2),
        Err(Error::NotSupported(_))
    ));
    assert!(matches!(
        layout.nonhead(2),
        Ok(NonHead { back: 2, blocks: 0 })
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
    let enabled = [
        cfg!(feature = "lz4"),
        cfg!(feature = "lzma"),
        cfg!(feature = "deflate"),
        cfg!(feature = "zstd"),
    ];
    for (algorithm, enabled) in enabled.into_iter().enumerate() {
        let mut data = image(algorithm as u8, &[]);
        let result = plan(&data, 0);
        if enabled {
            assert!(matches!(result, Ok(BlockPlan::Encoded(_))));
        } else {
            assert!(matches!(result, Err(Error::NotSupported(_))));
        }
        data[2118] <<= 4;
        data[2128] = 3;
        assert!(matches!(plan(&data, 0), Err(Error::CorruptedData(_))));
        data[1104] |= 8;
        assert_eq!(plan(&data, 0).is_ok(), enabled);
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
    assert!(matches!(plan(&data, 0), Err(Error::NotSupported(_))));
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

#[test]
fn codec_samples_require_complete_input_and_exact_output() {
    let wanted = expected();
    for (algorithm, encoding, payload) in samples() {
        let mut extent = EncodedExtent {
            offset: 0,
            size: 512,
            decoded_size: 1025,
            skip: 0,
            encoding,
        };
        let input = padded(payload);
        assert_eq!(
            extent.decode(&input).unwrap(),
            wanted,
            "algorithm {algorithm}"
        );
        extent.skip = 701;
        assert_eq!(extent.decode(&input).unwrap(), wanted[701..]);
        extent.skip = 0;
        for size in [1024, 1026] {
            extent.decoded_size = size;
            assert!(matches!(
                extent.decode(&input),
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
                matches!(extent.decode(&bad), Err(Error::CorruptedData(_))),
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
        let error = decode_microlzma(&input, &mut [0; 1], 32768).unwrap_err();
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
    assert!(decode_zstd(sample, &mut output, 1024).is_err());
    for (at, xor) in [(4, 8), (5, 1), (sample.len() - 1, 1)] {
        let mut bad = sample.to_vec();
        bad[at] ^= xor;
        assert!(matches!(
            decode_zstd(&bad, &mut output, 1 << 20),
            Err(Error::CorruptedData(_))
        ));
    }
    assert!(decode_zstd(&[sample, sample].concat(), &mut output, 1 << 20).is_err());
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
    };
    assert_eq!(extent.decode(b"abcdefgh").unwrap(), b"abcd");
}

struct Source<'a> {
    data: SliceImage<'a>,
    reads: AtomicUsize,
    fail: AtomicBool,
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
        let bytes = self
            .get(offset..offset + buf.len() as u64)
            .ok_or_else(|| Error::OutOfBounds("injected read failure".into()))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

fn ready<T>(future: impl Future<Output = T>) -> T {
    match core::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("in-memory backend must complete immediately"),
    }
}

#[test]
fn sync_async_block_assembly_cache_and_retry_contracts() {
    let wanted = [expected(), vec![b'C'; 475]].concat();
    for (algorithm, _, payload) in samples() {
        let data = image(algorithm, payload);
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
