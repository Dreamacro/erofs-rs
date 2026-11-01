use crate::source;
use anyhow::Result;
use bitflags::Flags;
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
// Filesystem blocks:                            5136
// Filesystem additional devices:                0
// Filesystem inode metadata start block:        0
// Filesystem shared xattr metadata start block: 0
// Filesystem root nid:                          37
// Filesystem sb_size:                           128
// Filesystem inode count:                       476
// Filesystem created:                           Thu Oct 22 20:33:33 2026
// Filesystem compatible features:               sb_csum mtime plain_xattr_pfx
// Filesystem incompatible features:             chunked_file xattr_prefixes
// Filesystem UUID:                              f980c99a-21e7-4bc2-9f37-cbf3f5cb69c9

fn feature_names(flags: impl Flags) -> Vec<&'static str> {
    flags
        .iter()
        .flat_map(|flag| flag.iter_equal_names())
        .collect()
}

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
        "Filesystem compatible features:               {}",
        feature_names(block.feature_compat_flags()).join(" ")
    );
    println!(
        "Filesystem incompatible features:             {}",
        feature_names(block.feature_incompat_flags()).join(" ")
    );

    println!(
        "Filesystem UUID:                              {}",
        Uuid::from_bytes(block.uuid)
    );

    Ok(())
}
