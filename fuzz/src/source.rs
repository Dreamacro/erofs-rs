use erofs_rs::{
    Error, Result,
    backend::{AsyncImage, Image},
};
use std::{
    future::Future,
    ops::{Bound, RangeBounds},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    task::{Context, Poll, Waker},
};

// Bound harness work, not on-disk fields. Budget exhaustion is not a parser failure.
#[derive(Default)]
pub struct State {
    pub reads: AtomicUsize,
    bytes: AtomicUsize,
    pub exhausted: AtomicBool,
    pub injected: AtomicBool,
    fail: AtomicUsize,
}

impl State {
    pub fn arm(&self, nth: Option<usize>) {
        self.injected.store(false, Relaxed);
        self.fail
            .store(nth.map_or(0, |n| self.reads.load(Relaxed) + n + 1), Relaxed);
    }

    fn allow(&self, size: usize) -> bool {
        let read = self.reads.fetch_add(1, Relaxed) + 1;
        if read == self.fail.load(Relaxed) {
            self.injected.store(true, Relaxed);
            return false;
        }
        let bytes = self.bytes.fetch_add(size, Relaxed);
        if read > 1024 || size > 64 * 1024 * 1024 || bytes > 64 * 1024 * 1024 - size {
            self.exhausted.store(true, Relaxed);
            return false;
        }
        true
    }
}

#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub data: &'a [u8],
    pub state: &'a State,
}

impl Image for Source<'_> {
    fn len(&self) -> u64 {
        (1 << 48) + self.data.len() as u64
    }

    fn get<R: RangeBounds<u64>>(&self, range: R) -> Option<&[u8]> {
        let start = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n.checked_add(1)?,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&n) => n.checked_add(1)?,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => self.len(),
        };
        let size = usize::try_from(end.checked_sub(start)?).ok()?;
        if !self.state.allow(size) {
            return None;
        }
        // Sparse copies exercise high byte and 48-bit block addresses without huge allocations.
        // Gaps are unavailable, never synthesized as an infinite supply of zeroes.
        let base = if start >= 1 << 48 {
            1 << 48
        } else if start >= 1 << 40 {
            1 << 40
        } else {
            0
        };
        self.data
            .get(usize::try_from(start - base).ok()?..usize::try_from(end - base).ok()?)
    }
}

impl AsyncImage for Source<'_> {
    async fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(Error::Overflow("fuzz read"))?;
        match self.get(offset..end) {
            Some(bytes) => {
                buf.copy_from_slice(bytes);
                Ok(())
            }
            None => {
                // An exact-read error is allowed to leave a partially modified buffer.
                if self.state.injected.load(Relaxed) {
                    buf[0] = 0xa5;
                }
                Err(Error::OutOfBounds("fuzz backend read".into()))
            }
        }
    }
}

pub fn ready<T>(future: impl Future<Output = T>) -> T {
    match std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("memory backend must complete immediately"),
    }
}
