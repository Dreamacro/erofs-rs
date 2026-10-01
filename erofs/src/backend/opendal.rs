use std::io::Read;

use alloc::string::String;

use opendal::{BytesRange, Operator, options::ReadOptions};

use super::AsyncImage;
use crate::{Error, Result};

pub struct OpendalImage(Operator, String);

impl OpendalImage {
    pub fn new(operator: Operator, path: String) -> Self {
        Self(operator, path)
    }
}

impl AsyncImage for OpendalImage {
    async fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let size = buf.len() as u64;
        offset
            .checked_add(size)
            .ok_or(Error::Overflow("image read range"))?;
        self.0
            .read_options(
                &self.1,
                ReadOptions {
                    range: BytesRange::new(offset, Some(size)),
                    ..Default::default()
                },
            )
            .await?
            .read_exact(buf)?;
        Ok(())
    }
}
