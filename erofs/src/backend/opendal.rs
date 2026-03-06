use std::io::Read;

use alloc::string::String;

use opendal::{Operator, options::ReadOptions, raw::BytesRange};

use super::AsyncImage;
use crate::Result;

pub struct OpendalImage(Operator, String);

impl OpendalImage {
    pub fn new(operator: Operator, path: String) -> Self {
        Self(operator, path)
    }
}

impl AsyncImage for OpendalImage {
    async fn read_exact_at(&self, buf: &mut [u8], offset: usize) -> Result<usize> {
        self.0
            .read_options(
                &self.1,
                ReadOptions {
                    range: BytesRange::new(offset as _, Some(buf.len() as _)),
                    ..Default::default()
                },
            )
            .await?
            .read_exact(buf)?;
        Ok(buf.len())
    }
}
