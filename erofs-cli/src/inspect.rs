use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local};
use clap::{Args, Subcommand};
use erofs_rs::{
    EroFS,
    backend::{Image, MmapImage},
    types::Inode,
};

#[derive(Args, Debug)]
pub struct InspectArgs {
    #[clap(short, long)]
    image: String,

    #[command(subcommand)]
    operation: InspectSubcommands,
}

#[derive(Subcommand, Debug)]
enum InspectSubcommands {
    Ls {
        #[clap(default_value = "/")]
        path: String,
    },
    Cat {
        path: String,
    },
}

pub fn inspect(args: InspectArgs) -> Result<()> {
    let image = MmapImage::new_from_path(args.image)?;
    let fs = EroFS::new(image)?;

    match args.operation {
        InspectSubcommands::Ls { path } => ls(&fs, &path)?,
        InspectSubcommands::Cat { path } => cat(&fs, &path)?,
    }

    Ok(())
}

fn format_mode(inode: &Inode) -> String {
    let mut res = String::with_capacity(10);
    res.push(if inode.is_dir() { 'd' } else { '-' });

    let masks = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'), // User
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'), // Group
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'), // Other
    ];

    let mode = inode.permissions().mode();
    for (mask, char) in masks {
        if mode & mask != 0 {
            res.push(char);
        } else {
            res.push('-');
        }
    }

    res
}

fn format_size(inode: &Inode) -> String {
    let size = inode.data_size();
    if size < 1024 {
        format!("{}B", size)
    } else if size < 1024 * 1024 {
        format!("{:.1}KiB", size as f64 / 1024.0)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:.1}MiB", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}GB", size as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_time(inode: &Inode) -> String {
    let t = match inode.modified() {
        Some(t) => t,
        None => return String::from(""),
    };

    let dt: DateTime<Local> = t.into();
    let now = Local::now();
    if dt.year() == now.year() {
        dt.format("%b %e %H:%M").to_string()
    } else {
        dt.format("%b %e  %Y").to_string()
    }
}

fn ls<I: Image>(fs: &EroFS<I>, path: &str) -> Result<()> {
    let read_dir = fs
        .read_dir(path)
        .with_context(|| format!("failed to read directory: {}", path))?;

    for entry in read_dir {
        let entry = entry.with_context(|| "failed to read directory entry")?;
        let inode = entry.inode;
        println!(
            "{} {:>8} {} {}",
            format_mode(&inode),
            format_size(&inode),
            format_time(&inode),
            entry.dir_entry.file_name()
        );
    }

    Ok(())
}

fn cat<I: Image>(fs: &EroFS<I>, path: &str) -> Result<()> {
    let mut file = fs
        .open(path)
        .with_context(|| format!("failed to open file: {}", path))?;

    if file.size() > 1024 * 1024 {
        return Err(anyhow::anyhow!("file too large to output (>{} MiB)", 1));
    }

    std::io::copy(&mut file, &mut std::io::stdout())?;
    Ok(())
}
