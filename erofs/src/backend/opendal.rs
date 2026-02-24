use opendal::Reader;

use super::AsyncImage;
use crate::Result;

pub struct OpendalImage(Reader);

impl OpendalImage {
    pub fn new(reader: Reader) -> Self {
        Self(reader)
    }
}

impl AsyncImage for OpendalImage {
    async fn read_at(&self, buf: &mut [u8], offset: usize) -> Result<usize> {
        let mut slice = &mut buf[..];
        let n = self.0.read_into(&mut slice, offset as u64..).await?;
        Ok(n)
    }
}

impl From<Reader> for OpendalImage {
    fn from(reader: Reader) -> Self {
        Self::new(reader)
    }
}
