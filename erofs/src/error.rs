use thiserror::Error;

#[cfg(feature = "std")]
use std::string::String;

#[cfg(not(feature = "std"))]
use alloc::string::String;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid super block: {0}")]
    InvalidSuperblock(String),

    #[error("invalid dirent file type: {0}")]
    InvalidDirentFileType(u8),

    #[error("invalid layout: {0}")]
    InvalidLayout(u8),

    #[error("path not found: {0}")]
    PathNotFound(String),

    #[error("not a file: {0}")]
    NotAFile(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),

    #[error("out of bounds: {0}")]
    OutOfBounds(String),

    #[error("binread error: {0}")]
    BinRead(#[from] binrw::Error),

    #[error("out of range {0} of {1}")]
    OutOfRange(usize, usize),

    #[error("{0} not supported yet")]
    NotSupported(String),

    #[error("corrupted data: {0}")]
    CorruptedData(String),
}

#[cfg(feature = "std")]
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(not(feature = "std"))]
pub type Result<T> = core::result::Result<T, Error>;
