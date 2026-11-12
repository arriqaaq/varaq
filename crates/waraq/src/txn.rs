pub mod clock;
pub mod commits;
pub mod db;
pub mod tuple;
pub mod versions;
pub mod visible;
use core::sync::atomic::{AtomicU64, Ordering};
const IDLE: u64 = u64::MAX;
const BEGINNING: u64 = u64::MAX - 1;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerId(pub usize);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);
impl Timestamp {
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        if raw >= BEGINNING { None } else { Some(Self(raw)) }
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watermark {
    Empty,
    Oldest(Timestamp),
    Beginning,
}
#[derive(Debug)]
pub struct Snapshots {
    slots: Box<[Slot]>,
}
#[derive(Debug)]
#[repr(align(128))]
struct Slot(AtomicU64, AtomicU64);
impl core::ops::Deref for Slot {
    type Target = AtomicU64;
    fn deref(&self) -> &AtomicU64 {
        &self.0
    }
}
impl Snapshots {
    #[must_use]
    pub fn new(workers: usize) -> Self {
        assert!(workers > 0, "a snapshot registry needs at least one worker");
        let slots =
            (0..workers).map(|_| Slot(AtomicU64::new(IDLE), AtomicU64::new(0))).collect::<Vec<_>>();
        Self { slots: slots.into_boxed_slice() }
    }
    #[must_use]
    pub fn workers(&self) -> usize {
        self.slots.len()
    }
    #[must_use]
    pub fn beginning(&self, worker: WorkerId) -> bool {
        self.slots[worker.0]
            .compare_exchange(IDLE, BEGINNING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
    #[must_use]
    pub fn is_idle(&self, worker: WorkerId) -> bool {
        self.slots[worker.0].load(Ordering::Acquire) == IDLE
    }
    #[must_use]
    pub fn watermark(&self, _asking: WorkerId) -> Watermark {
        let mut oldest = IDLE;
        for slot in &self.slots {
            let seen = slot.load(Ordering::SeqCst);
            if seen == BEGINNING {
                return Watermark::Beginning;
            }
            if seen < oldest {
                oldest = seen;
            }
        }
        Timestamp::new(oldest).map_or(Watermark::Empty, Watermark::Oldest)
    }
    pub fn publish(&self, worker: WorkerId, at: Timestamp) {
        self.slots[worker.0].store(at.get(), Ordering::SeqCst);
    }
    #[must_use]
    pub fn holds(&self, worker: WorkerId, at: Timestamp) -> bool {
        self.slots[worker.0].load(Ordering::Acquire) == at.get()
    }
    pub fn release(&self, worker: WorkerId) {
        self.slots[worker.0].store(IDLE, Ordering::Release);
    }
    #[must_use = "the count is the cadence; counting without reading it paces nothing"]
    pub(crate) fn read_only_commit(&self, worker: WorkerId) -> u64 {
        let count = &self.slots[worker.0].1;
        let next = count.load(Ordering::Relaxed).wrapping_add(1);
        count.store(next, Ordering::Relaxed);
        next
    }
    #[must_use]
    pub fn oldest(&self, _asking: WorkerId) -> Option<Timestamp> {
        let mut oldest = IDLE;
        for slot in &self.slots {
            let seen = slot.load(Ordering::Acquire);
            if seen < oldest {
                oldest = seen;
            }
        }
        Timestamp::new(oldest)
    }
    #[must_use]
    pub fn collectable(&self, asking: WorkerId, superseded_at: Timestamp) -> bool {
        match self.watermark(asking) {
            Watermark::Empty => true,
            Watermark::Oldest(watermark) => superseded_at <= watermark,
            Watermark::Beginning => false,
        }
    }
}
