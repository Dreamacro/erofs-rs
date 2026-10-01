use alloc::string::String;
use thiserror::Error;

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

    #[error("integer overflow: {0}")]
    Overflow(&'static str),

    #[error("not a symbolic link: {0}")]
    NotASymlink(u64),

    #[error("binread error: {0}")]
    BinRead(#[cfg_attr(feature = "std", from)] binrw::Error),

    #[error("out of range {0} of {1}")]
    OutOfRange(usize, usize),

    #[error("{0} not supported yet")]
    NotSupported(String),

    #[error("corrupted data: {0}")]
    CorruptedData(String),

    #[cfg(feature = "std")]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "opendal")]
    #[error("opendal error: {0}")]
    Opendal(#[from] opendal::Error),
}

// binrw only implements the Error trait with std; preserve the value without
// treating it as an error source in no_std builds.
#[cfg(not(feature = "std"))]
impl From<binrw::Error> for Error {
    fn from(error: binrw::Error) -> Self {
        Self::BinRead(error)
    }
}

pub type Result<T> = core::result::Result<T, Error>;
