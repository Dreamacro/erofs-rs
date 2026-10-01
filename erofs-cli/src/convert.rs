use std::{
    ffi::OsStr,
    fs::File,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use erofs_rs::{EroFS, backend::MmapImage, types::Inode};
use tar::{EntryType, Header};

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
    ensure!(
        args.format.as_deref().is_none_or(|format| format == "tar"),
        "only tar output is supported"
    );
    let input = File::open(&args.path)?;
    let input_meta = input.metadata()?;
    // SAFETY: input immutability is a CLI precondition. The identity check below
    // prevents this command from truncating its own input.
    let image = unsafe { MmapImage::new_from_file(&input)? };
    let fs = EroFS::new(image)?;
    let entries = fs.walk_dir(&args.root)?;

    // Check file identity before truncating, including hard links to the input.
    let out_file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&args.output)?;
    let output_meta = out_file.metadata()?;
    ensure!(
        (input_meta.dev(), input_meta.ino()) != (output_meta.dev(), output_meta.ino()),
        "input and output refer to the same file"
    );
    if output_meta.is_file() {
        out_file.set_len(0)?;
    }
    let mut tar = tar::Builder::new(out_file);

    for entry in entries {
        let entry = entry.context("read entry failed")?;
        let image_path = entry.dir_entry.path();
        let relative_path = image_path.strip_prefix("/").unwrap_or(image_path.as_path());
        let path = Path::new(OsStr::from_bytes(relative_path.as_bytes()));
        let file_type = entry.inode.file_type();

        let mut header = Header::new_gnu();
        header.set_mode(entry.inode.permissions().mode() & 0o7777);
        header.set_uid(u64::from(entry.inode.uid()));
        header.set_gid(u64::from(entry.inode.gid()));
        let mtime = match entry.inode {
            Inode::Compact(_) => fs.super_block().build_time,
            Inode::Extended(_) => entry
                .inode
                .modified()
                .context("invalid inode modification time")?
                .duration_since(UNIX_EPOCH)?
                .as_secs(),
        };
        header.set_mtime(mtime);
        header.set_size(0);

        if file_type.is_file() {
            header.set_entry_type(EntryType::Regular);
            header.set_size(entry.inode.data_size() as u64);
            tar.append_data(&mut header, path, fs.open_inode_file(entry.inode)?)?;
        } else if file_type.is_symlink() {
            header.set_entry_type(EntryType::Symlink);
            let target = fs.read_link_inode(entry.inode)?;
            tar.append_link(
                &mut header,
                path,
                Path::new(OsStr::from_bytes(target.as_bytes())),
            )?;
        } else {
            let entry_type = if file_type.is_dir() {
                EntryType::Directory
            } else if file_type.is_fifo() {
                EntryType::Fifo
            } else if file_type.is_char_device() {
                EntryType::Char
            } else if file_type.is_block_device() {
                EntryType::Block
            } else {
                bail!(
                    "cannot represent {:?} in tar: {}",
                    file_type,
                    path.display()
                );
            };
            header.set_entry_type(entry_type);
            if file_type.is_char_device() || file_type.is_block_device() {
                // Device inodes store Linux new_encode_dev, not a data block address.
                let device = entry.inode.raw_block_addr();
                header.set_device_major((device >> 8) & 0xfff)?;
                header.set_device_minor((device & 0xff) | ((device >> 12) & 0xfff00))?;
            }
            tar.append_data(&mut header, path, std::io::empty())?;
        }
    }

    tar.finish().context("failed to finish tar archive")?;
    Ok(())
}
