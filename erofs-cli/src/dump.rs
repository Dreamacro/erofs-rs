use anyhow::Result;
use clap::Args;

use chrono::{DateTime, Local};
use erofs_rs::{
    EroFS,
    backend::MmapImage,
    types::{SB_EXTSLOT_SIZE, SuperBlock},
};
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct DumpArgs {
    path: String,
}

// Filesystem magic number:                      0xE0F5E1E2
// Filesystem blocksize:                         4096
// Filesystem blocks:                            55
// Filesystem inode metadata start block:        0
// Filesystem shared xattr metadata start block: 0
// Filesystem root nid:                          38
// Filesystem lz4_max_distance:                  0
// Filesystem sb_size:                           128
// Filesystem inode count:                       516
// Filesystem created:                           Fri Dec  5 00:48:29 2025
// Filesystem features:                          sb_csum mtime xattr_filter
// Filesystem UUID:                              71bd9ab4-fb8c-47b4-986c-5c901ad547c7

pub fn dump(args: DumpArgs) -> Result<()> {
    let image = MmapImage::new_from_path(args.path)?;
    let fs = EroFS::new(image.into())?;

    let block = fs.super_block();

    println!(
        "Filesystem magic number:                      {:#X}",
        block.magic
    );
    println!(
        "Filesystem blocksize:                         {}",
        1 << block.blk_size_bits
    );
    println!(
        "Filesystem blocks:                            {}",
        block.blocks
    );
    println!(
        "Filesystem inode metadata start block:        {}",
        block.meta_blk_addr
    );
    println!(
        "Filesystem shared xattr metadata start block: {}",
        block.xattr_blk_addr
    );
    println!(
        "Filesystem root nid:                          {}",
        block.root_nid
    );
    // println!(
    //     "Filesystem lz4_max_distance:                  {}",
    //     block.lz4_max_distance
    // );
    println!(
        "Filesystem sb_size:                           {}",
        SuperBlock::size() + block.ext_slots as usize * SB_EXTSLOT_SIZE
    );
    println!(
        "Filesystem inode count:                       {}",
        block.inos
    );
    println!(
        "Filesystem created:                           {}",
        DateTime::from_timestamp(block.build_time as i64, block.build_time_ns)
            .unwrap()
            .with_timezone(&Local)
            .format("%a %b %e %H:%M:%S %Y")
    );
    println!(
        "Filesystem features:                          {}",
        block.feature_compat
    );

    println!(
        "Filesystem UUID:                              {}",
        Uuid::from_bytes(block.uuid)
    );

    Ok(())
}
