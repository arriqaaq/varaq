use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
const IN_FLIGHT: u32 = 0xFFFF;
const BEGUN: u32 = 1 << 16;
use crate::PageId;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Free = 0,
    Loading = 1,
    Hot = 2,
    Cool = 3,
}
impl State {
    const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Loading),
            2 => Some(Self::Hot),
            3 => Some(Self::Cool),
            _ => None,
        }
    }
    #[must_use]
    pub const fn is_readable(self) -> bool {
        matches!(self, Self::Hot | Self::Cool)
    }
}
#[derive(Debug, Default)]
pub struct ParentHint {
    frame: AtomicU32,
    slot: AtomicU32,
    version: AtomicU64,
}
const NO_PARENT: u32 = u32::MAX;
pub const NO_WRITER: u32 = u32::MAX;
pub const NO_FRAME: u32 = u32::MAX;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reissue {
    Yes,
    No,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample(u64);
impl Sample {
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn state(self) -> State {
        State::from_u8((self.0 & STATE_ONLY) as u8)
            .expect("a buffer frame holds one of four states")
    }
    #[must_use]
    pub const fn version(self) -> u64 {
        self.0 >> STATE_BITS
    }
}
impl ParentHint {
    pub fn record(&self, frame: u32, slot: u32, version: u64) {
        if self.version.load(Ordering::Acquire) == version
            && self.frame.load(Ordering::Acquire) == frame
            && self.slot.load(Ordering::Acquire) == slot
        {
            return;
        }
        self.frame.store(frame, Ordering::Release);
        self.slot.store(slot, Ordering::Release);
        self.version.store(version, Ordering::Release);
    }
    #[must_use]
    pub fn get(&self) -> Option<(u32, u32, u64)> {
        let frame = self.frame.load(Ordering::Acquire);
        if frame == NO_PARENT {
            return None;
        }
        let slot = self.slot.load(Ordering::Acquire);
        let version = self.version.load(Ordering::Acquire);
        Some((frame, slot, version))
    }
    pub fn clear(&self) {
        self.frame.store(NO_PARENT, Ordering::Release);
    }
}
const FIELD_STORE: Ordering = Ordering::Release;
const FIELD_LOAD: Ordering = Ordering::Acquire;
const STATE_BITS: u32 = 8;
const STATE_MASK: u64 = (1 << STATE_BITS) - 1;
const STATE_ONLY: u64 = STATE_MASK;
#[derive(Debug)]
pub struct BufferFrame {
    word: AtomicU64,
    pid: AtomicU64,
    gsn: AtomicU64,
    on_disk: AtomicU64,
    last_writer: AtomicU32,
    writing: AtomicU32,
    pub parent: ParentHint,
}
impl Default for BufferFrame {
    fn default() -> Self {
        Self::new()
    }
}
impl BufferFrame {
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new() -> Self {
        Self {
            word: AtomicU64::new(State::Free as u64),
            pid: AtomicU64::new(0),
            gsn: AtomicU64::new(0),
            on_disk: AtomicU64::new(0),
            last_writer: AtomicU32::new(NO_WRITER),
            writing: AtomicU32::new(0),
            parent: ParentHint {
                frame: AtomicU32::new(NO_PARENT),
                slot: AtomicU32::new(0),
                version: AtomicU64::new(0),
            },
        }
    }
    #[must_use]
    pub fn sample(&self) -> Sample {
        Sample(self.word.load(Ordering::Acquire))
    }
    #[must_use]
    pub fn recheck(&self, sampled: Sample) -> bool {
        fence(Ordering::Acquire);
        self.word.load(Ordering::Relaxed) == sampled.0
    }
    #[must_use]
    pub fn state(&self) -> State {
        self.sample().state()
    }
    #[must_use]
    pub fn version(&self) -> u64 {
        self.sample().version()
    }
    #[must_use]
    pub fn page_id(&self) -> Option<PageId> {
        if self.state() == State::Free {
            return None;
        }
        PageId::new(self.pid.load(FIELD_LOAD)).ok()
    }
    #[must_use]
    pub fn gsn(&self) -> u64 {
        self.gsn.load(FIELD_LOAD)
    }
    #[must_use]
    pub fn last_writer(&self) -> u32 {
        self.last_writer.load(FIELD_LOAD)
    }
    pub fn written_by(&self, worker: u32) {
        self.last_writer.store(worker, FIELD_STORE);
    }
    pub fn take_over(&self, source: &Self, between: impl FnOnce()) -> Result<(), State> {
        let Self { word: _, pid: _, gsn, on_disk, last_writer, writing, parent } = source;
        if writing.load(FIELD_LOAD) & IN_FLIGHT != 0 {
            return Err(source.state());
        }
        self.arrive(on_disk.load(FIELD_LOAD))?;
        between();
        self.modified(gsn.load(FIELD_LOAD));
        self.written_by(last_writer.load(FIELD_LOAD));
        if let Some((frame, slot, version)) = parent.get() {
            self.parent.record(frame, slot, version);
        }
        Ok(())
    }
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        calc::dirty(self.gsn.load(FIELD_LOAD), self.on_disk.load(FIELD_LOAD))
    }
    #[must_use]
    pub fn on_disk(&self) -> u64 {
        self.on_disk.load(FIELD_LOAD)
    }
    #[must_use]
    pub fn writebacks_in_flight(&self) -> u32 {
        self.writing.load(Ordering::Acquire) & IN_FLIGHT
    }
    #[must_use]
    pub fn writeback_in_flight(&self) -> Option<u64> {
        let word = self.writing.load(Ordering::Acquire);
        (word & IN_FLIGHT > 0).then_some(u64::from(word >> 16))
    }
    pub fn begin_load(&self, page: PageId) -> Result<(), State> {
        self.transition(State::Free, State::Loading, Reissue::Yes)?;
        self.pid.store(page.get(), FIELD_STORE);
        self.gsn.store(0, FIELD_STORE);
        self.on_disk.store(0, FIELD_STORE);
        self.parent.clear();
        Ok(())
    }
    pub fn arrive(&self, disk_gsn: u64) -> Result<(), State> {
        self.last_writer.store(NO_WRITER, FIELD_STORE);
        self.gsn.store(disk_gsn, FIELD_STORE);
        self.on_disk.store(disk_gsn, FIELD_STORE);
        self.transition(State::Loading, State::Hot, Reissue::Yes)
    }
    pub fn abandon_load(&self) -> Result<(), State> {
        self.free(&[State::Loading])
    }
    pub fn cool(&self) -> Result<(), State> {
        self.transition(State::Hot, State::Cool, Reissue::No)
    }
    pub fn warm(&self) -> Result<(), State> {
        self.transition(State::Cool, State::Hot, Reissue::No)
    }
    pub fn reclaim(&self) -> Result<(), State> {
        if self.writebacks_in_flight() > 0 {
            return Err(self.state());
        }
        self.free(&[State::Hot, State::Cool])?;
        self.parent.clear();
        Ok(())
    }
    pub fn evict(&self) -> Result<(), State> {
        self.free(&[State::Cool])?;
        self.parent.clear();
        Ok(())
    }
    pub fn modified(&self, gsn: u64) {
        self.gsn.fetch_max(gsn, Ordering::Release);
    }
    pub fn begin_writeback(&self) {
        self.writing.fetch_add(BEGUN | 1, Ordering::AcqRel);
    }
    pub fn end_writeback(&self, gsn: u64) {
        self.on_disk.fetch_max(gsn, Ordering::Release);
        self.writing.fetch_sub(1, Ordering::AcqRel);
    }
    fn free(&self, from: &[State]) -> Result<(), State> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let found = Sample(current).state();
            if !from.contains(&found) {
                return Err(found);
            }
            let version = Sample(current).version() + 1;
            let next = (version << STATE_BITS) | (State::Free as u64);
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(());
                }
                Err(actual) => current = actual,
            }
        }
    }
    fn transition(&self, from: State, to: State, reissue: Reissue) -> Result<(), State> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let found = Sample(current).state();
            if found != from {
                return Err(found);
            }
            let version = Sample(current).version() + u64::from(reissue == Reissue::Yes);
            let next = (version << STATE_BITS) | (to as u64);
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(());
                }
                Err(actual) => current = actual,
            }
        }
    }
}
