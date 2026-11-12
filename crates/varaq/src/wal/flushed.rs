use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[derive(Debug)]
pub struct Flushed {
    slots: Box<[crate::sync::Padded<AtomicU64>]>,
    stopped: AtomicBool,
    held: crate::stats::Stat,
}
impl Flushed {
    #[must_use]
    pub fn new(workers: usize) -> Self {
        assert!(workers > 0, "a flush registry needs at least one worker");
        let slots =
            (0..workers).map(|_| crate::sync::Padded(AtomicU64::new(0))).collect::<Vec<_>>();
        Self {
            slots: slots.into_boxed_slice(),
            stopped: AtomicBool::new(false),
            held: crate::stats::Stat::default(),
        }
    }
    #[must_use]
    pub fn workers(&self) -> usize {
        self.slots.len()
    }
    #[must_use = "whether the cursor moved is what decides if a writeback pass is worth running"]
    pub fn publish(&self, worker: usize, gsn: u64) -> bool {
        let before = self.slots[worker].fetch_max(gsn, Ordering::Release);
        gsn > before
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    #[must_use]
    pub fn of(&self, worker: usize) -> u64 {
        self.slots[worker].load(Ordering::Acquire)
    }
    #[must_use]
    pub fn global_min(&self) -> u64 {
        self.slots.iter().map(|s| s.load(Ordering::Acquire)).min().unwrap_or(0)
    }
    #[must_use]
    pub fn may_write_back(&self, page_gsn: u64) -> bool {
        let may = !self.is_stopped() && page_gsn <= self.global_min();
        if !may {
            self.held.add(1);
        }
        may
    }
    #[must_use]
    pub fn held_back(&self) -> u64 {
        self.held.get()
    }
}
