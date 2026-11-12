use crate::frame::State;
use std::sync::Mutex;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Cool,
    Skip(Skipped),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    NotHot,
    AlreadyCooling,
    BeingWritten,
    QueueFull,
    LogBehind,
}
#[must_use]
pub fn examine(
    state: State,
    writebacks_in_flight: u32,
    log_covers: bool,
    queue_has_room: bool,
) -> Verdict {
    if state == State::Cool {
        return Verdict::Skip(Skipped::AlreadyCooling);
    }
    if state != State::Hot {
        return Verdict::Skip(Skipped::NotHot);
    }
    if writebacks_in_flight > 0 {
        return Verdict::Skip(Skipped::BeingWritten);
    }
    if !log_covers {
        return Verdict::Skip(Skipped::LogBehind);
    }
    if queue_has_room { Verdict::Cool } else { Verdict::Skip(Skipped::QueueFull) }
}
#[derive(Debug)]
#[repr(align(64))]
pub struct CoolingStage {
    ring: Mutex<Ring>,
}
#[derive(Debug)]
struct Ring {
    slots: Box<[u32]>,
    head: usize,
    len: usize,
}
impl CoolingStage {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a cooling stage with no capacity can never evict anything");
        Self {
            ring: Mutex::new(Ring { slots: vec![0; capacity].into_boxed_slice(), head: 0, len: 0 }),
        }
    }
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.ring.lock().map_or(0, |r| r.slots.len())
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.lock().map_or(0, |r| r.len)
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.ring.lock().is_ok_and(|r| r.len < r.slots.len())
    }
    pub fn admit(&self, frame: u32) -> bool {
        let Ok(mut r) = self.ring.lock() else { return false };
        if r.len == r.slots.len() {
            return false;
        }
        let at = (r.head + r.len) % r.slots.len();
        r.slots[at] = frame;
        r.len += 1;
        true
    }
    pub fn take_oldest(&self) -> Option<u32> {
        let Ok(mut r) = self.ring.lock() else { return None };
        if r.len == 0 {
            return None;
        }
        let frame = r.slots[r.head];
        r.head = (r.head + 1) % r.slots.len();
        r.len -= 1;
        Some(frame)
    }
    #[must_use]
    pub fn below_target(&self, target: usize) -> bool {
        self.len() < target.min(self.capacity())
    }
}
#[must_use]
pub const fn capacity_for(frames: usize, percent: usize) -> usize {
    let n = frames * percent / 100;
    if n == 0 { 1 } else { n }
}
pub const DEFAULT_COOLING_PERCENT: usize = 10;
