use core::sync::atomic::{AtomicU64, Ordering};
#[derive(Debug, Default)]
pub struct Clock {
    now: AtomicU64,
}
impl Clock {
    #[must_use]
    pub fn new() -> Self {
        Self { now: AtomicU64::new(0) }
    }
    #[must_use]
    pub fn now(&self) -> u64 {
        self.now.load(Ordering::Acquire)
    }
    pub fn observe(&self, page_gsn: u64) -> u64 {
        let before = self.now.fetch_max(page_gsn, Ordering::AcqRel);
        before.max(page_gsn)
    }
    pub fn stamp(&self, page_gsn: u64) -> Option<u64> {
        self.observe(page_gsn);
        self.now
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| now.checked_add(1))
            .ok()
            .map(|before| before + 1)
    }
}
