use anyhow::Result;
use chrono::{DateTime, Local};
use clap::Args;
use erofs_rs::{
    EroFS,
    r#async::EroFS as AsyncEroFS,
    backend::MmapImage,
    backend::OpendalImage,
    types::{SB_EXTSLOT_SIZE, SuperBlock},
};
use opendal::{Operator, services};
use url::{Position, Url};
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

pub async fn dump(args: DumpArgs) -> Result<()> {
    let block = if args.path.starts_with("http") {
        let u = Url::parse(&args.path)?;
        let builder = services::Http::default().endpoint(&u[..Position::BeforePath]);
        let op = Operator::new(builder)?.finish();
        let image = OpendalImage::new(op, u.path().to_string());
        let fs = AsyncEroFS::new(image).await?;
        fs.super_block().to_owned()
    } else {
        let image = MmapImage::new_from_path(args.path)?;
        let fs = EroFS::new(image)?;
        fs.super_block().to_owned()
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
