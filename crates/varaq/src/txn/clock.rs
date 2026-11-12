use crate::Error;
use crate::txn::Timestamp;
use core::sync::atomic::{AtomicU64, Ordering};
#[derive(Debug)]
pub struct Clock {
    next: AtomicU64,
}
impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}
impl Clock {
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(1)
    }
    #[must_use]
    pub fn starting_at(raw: u64) -> Self {
        Self { next: AtomicU64::new(raw) }
    }
    pub fn take(&self) -> Result<Timestamp, Error> {
        let mut seen = self.next.load(Ordering::Acquire);
        loop {
            let Some(stamped) = Timestamp::new(seen) else { return Err(Error::Corrupt) };
            match self.next.compare_exchange_weak(
                seen,
                seen + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(stamped),
                Err(now) => seen = now,
            }
        }
    }
    pub fn snapshot(&self) -> Result<Timestamp, Error> {
        Timestamp::new(self.next.load(Ordering::SeqCst)).ok_or(Error::Corrupt)
    }
    #[must_use]
    pub fn peek(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }
}
