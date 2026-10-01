use anyhow::{Result, ensure};
use erofs_rs::backend::{MmapImage, OpendalImage};
use opendal::{Operator, services};
use url::{Position, Url};

pub fn is_remote(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

pub fn http_image(path: &str) -> Result<OpendalImage> {
    ensure!(is_remote(path), "local and HTTP images cannot be mixed");
    let url = Url::parse(path)?;
    let builder = services::Http::default().endpoint(&url[..Position::BeforePath]);
    Ok(OpendalImage::new(
        Operator::new(builder)?,
        url.path().to_string(),
    ))
}

pub fn mmap_image(path: &str) -> Result<MmapImage> {
    ensure!(!is_remote(path), "local and HTTP images cannot be mixed");
    // SAFETY: the CLI requires every backing image to remain immutable while mapped.
    Ok(unsafe { MmapImage::new_from_path(path)? })
}
