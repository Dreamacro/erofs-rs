use crate::source;
use anyhow::Result;
use chrono::{DateTime, Local};
use clap::Args;
use erofs_rs::{
    EroFS,
    r#async::EroFS as AsyncEroFS,
    types::{SB_EXTSLOT_SIZE, SuperBlock},
};
use uuid::Uuid;

#[derive(Args, Debug)]
pub struct DumpArgs {
    path: String,
    /// Additional images in device-table order (repeat for each device).
    #[clap(long = "device", value_name = "PATH_OR_URL")]
    devices: Vec<String>,
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

pub async fn dump(args: DumpArgs) -> Result<()> {
    let (block, devices) = if source::is_remote(&args.path) {
        let image = source::http_image(&args.path)?;
        let devices = args
            .devices
            .iter()
            .map(|path| source::http_image(path))
            .collect::<Result<Vec<_>>>()?;
        let fs = AsyncEroFS::new_with_devices(image, devices).await?;
        (*fs.super_block(), fs.devices().to_vec())
    } else {
        let image = source::mmap_image(&args.path)?;
        let devices = args
            .devices
            .iter()
            .map(|path| source::mmap_image(path))
            .collect::<Result<Vec<_>>>()?;
        let fs = EroFS::new_with_devices(image, devices)?;
        (*fs.super_block(), fs.devices().to_vec())
    };

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
        block.block_count()
    );
    println!(
        "Filesystem additional devices:                {}",
        devices.len()
    );
    for (index, device) in devices.iter().enumerate() {
        println!(
            "Device {}: blocks={}, unified_start={}, tag=b\"{}\"",
            index + 1,
            device.blocks,
            device.unified_start_block,
            device.tag.escape_ascii()
        );
    }
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
        block.root_inode_id()
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
    let created = block
        .created_unix()
        .and_then(|(seconds, nanos)| DateTime::from_timestamp(seconds, nanos))
        .map(|dt| dt.with_timezone(&Local))
        .and_then(|dt| dt.naive_utc().checked_add_offset(*dt.offset()))
        .map(|dt| dt.format("%a %b %e %H:%M:%S %Y").to_string())
        .unwrap_or_else(|| "<invalid timestamp>".to_string());
    println!("Filesystem created:                           {}", created);
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
