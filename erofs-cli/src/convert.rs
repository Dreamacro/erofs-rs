use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::File,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use erofs_rs::{EroFS, backend::MmapImage};
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
    /// Additional local images in device-table order (repeat for each device).
    #[clap(long = "device", value_name = "PATH")]
    devices: Vec<String>,
}

pub fn convert(args: ConvertArgs) -> Result<()> {
    ensure!(
        args.format.as_deref().is_none_or(|format| format == "tar"),
        "only tar output is supported"
    );
    let input = File::open(&args.path)?;
    let mut input_meta = vec![input.metadata()?];
    // SAFETY: input immutability is a CLI precondition. The identity check below
    // covers every backing image before the output can be truncated.
    let image = unsafe { MmapImage::new_from_file(&input)? };
    let mut devices = Vec::new();
    for path in &args.devices {
        let file = File::open(path)?;
        input_meta.push(file.metadata()?);
        // SAFETY: the same immutability and output-alias checks apply to each device.
        devices.push(unsafe { MmapImage::new_from_file(&file)? });
    }
    let fs = EroFS::new_with_devices(image, devices)?;
    let entries = fs.walk_dir(format!("/{}", args.root))?;

    // Check file identity before truncating, including hard links to the input.
    let out_file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&args.output)?;
    let output_meta = out_file.metadata()?;
    ensure!(
        input_meta
            .iter()
            .all(|meta| (meta.dev(), meta.ino()) != (output_meta.dev(), output_meta.ino())),
        "input and output refer to the same file"
    );
    if output_meta.is_file() {
        out_file.set_len(0)?;
    }
    let mut tar = tar::Builder::new(out_file);
    let mut hardlinks = HashMap::new();

    for entry in entries {
        let entry = entry.context("read entry failed")?;
        // Normalize archive names only after filesystem lookup validated the root.
        let image_path = entry.dir_entry.path().normalize();
        let relative_path = image_path.strip_prefix("/").unwrap_or(image_path.as_path());
        let path = Path::new(OsStr::from_bytes(relative_path.as_bytes()));
        let file_type = entry.inode.file_type();
        let has_links = !file_type.is_dir() && entry.inode.nlink() > 1;

        let mut header = Header::new_gnu();
        header.set_mode(entry.inode.permissions().mode());
        header.set_uid(u64::from(entry.inode.uid()));
        header.set_gid(u64::from(entry.inode.gid()));
        let (mtime, nanos) = entry.inode.modified_unix();
        header.set_mtime(mtime.max(0) as u64);
        if mtime < 0 || nanos != 0 {
            // The unsigned tar header cannot represent pre-epoch or fractional times.
            let total = i128::from(mtime) * 1_000_000_000 + i128::from(nanos);
            let magnitude = total.unsigned_abs();
            let timestamp = format!(
                "{}{}.{:09}",
                if total < 0 { "-" } else { "" },
                magnitude / 1_000_000_000,
                magnitude % 1_000_000_000,
            );
            tar.append_pax_extensions([("mtime", timestamp.as_bytes())])?;
        }
        header.set_size(0);

        if has_links && let Some(target) = hardlinks.get(&entry.inode.id()) {
            header.set_entry_type(EntryType::Link);
            tar.append_link(&mut header, path, target)?;
            continue;
        }

        if file_type.is_file() {
            header.set_entry_type(EntryType::Regular);
            header.set_size(entry.inode.data_size());
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
            if let Some((major, minor)) = entry.inode.device() {
                header.set_device_major(major)?;
                header.set_device_minor(minor)?;
            }
            tar.append_data(&mut header, path, std::io::empty())?;
        }
        if has_links {
            hardlinks.insert(entry.inode.id(), path.to_path_buf());
        }
    }

    tar.finish().context("failed to finish tar archive")?;
    Ok(())
}
