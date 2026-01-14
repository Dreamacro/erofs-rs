use std::{fs::File, os::unix::fs::PermissionsExt, path::Path, time::UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use erofs_rs::EroFS;
use memmap2::Mmap;
use tar::Header;

#[derive(Args, Debug)]
pub struct ConvertArgs {
    path: String,
    #[clap(short, long, default_value = "/")]
    root: String,
    #[clap(short, long)]
    output: String,
    #[clap(short, long)]
    format: Option<String>,
}

pub fn convert(args: ConvertArgs) -> Result<()> {
    let file = File::open(args.path)?;
    let file = unsafe { Mmap::map(&file) }?;
    let fs = EroFS::new(file)?;

    let out_file = File::create(args.output)?;
    let mut tar = tar::Builder::new(out_file);

    for entry in fs.walk_dir(Path::new(&args.root))? {
        let entry = entry.context("read entry failed")?;

        let mut header = Header::new_gnu();
        header.set_path(entry.dir_entry.path().strip_prefix("/")?)?;
        header.set_mode(entry.inode.permissions().mode());
        if let Some(time) = entry.inode.modified() {
            header.set_mtime(time.duration_since(UNIX_EPOCH)?.as_secs());
        }

        if entry.dir_entry.file_type().is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            tar.append(&header, std::io::empty())?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(entry.inode.data_size() as u64);
            header.set_cksum();

            tar.append(&header, fs.open_inode_file(entry.inode)?)?;
        }
    }

    Ok(())
}
