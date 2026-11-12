#![allow(unsafe_code)]
use crate::bufmgr::cooling::{self, CoolingStage};
use crate::bufmgr::freelist::FreeList;
use crate::bufmgr::inflight::{Claim, InflightTable};
use crate::frame::{BufferFrame, State};
use crate::latch::HybridLatch;
use crate::node::{Node, PAGE_SIZE, PageKind};
use crate::{Error, PageId};
const NODE_WORDS: usize = PAGE_SIZE / 8;
pub type PageLatch = HybridLatch<Node, NODE_WORDS>;
pub type PageGuard<'a> = crate::latch::ExclusiveGuard<'a, Node, NODE_WORDS>;
#[repr(align(64))]
pub struct Slot {
    pub frame: BufferFrame,
    pub page: PageLatch,
}
pub type Reissuable = std::sync::Arc<crate::sync::Mutex<Vec<(PageId, u64)>>>;
pub struct Pool {
    slots: Slots,
    partitions: Box<[FreeList]>,
    cooling: Box<[CoolingStage]>,
    inflight: Box<[InflightTable]>,
    moved_in: Box<[core::sync::atomic::AtomicU64]>,
    captures: Box<[core::sync::atomic::AtomicU64]>,
    logged: Box<[core::sync::atomic::AtomicU64]>,
    retired: crate::sync::Mutex<Vec<Retired>>,
    reissuable: Reissuable,
    vouches: Box<[crate::sync::Mutex<Vouch>]>,
}
#[derive(Debug, Default)]
pub(crate) struct Vouch {
    mark: Vec<u64>,
    pending: Vec<u64>,
    covered: Vec<u64>,
}
impl Vouch {
    fn vouched(&self, w: usize) -> u64 {
        let at = |v: &Vec<u64>| v.get(w).copied().unwrap_or(u64::MAX);
        at(&self.mark).min(at(&self.pending)).min(at(&self.covered))
    }
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retired {
    pub(crate) partition: usize,
    pub(crate) page: PageId,
    pub(crate) gsn: u64,
    pub(crate) kind: u8,
}
struct Slots {
    base: *mut Slot,
    len: usize,
}
unsafe impl Send for Slots {}
unsafe impl Sync for Slots {}
impl Slots {
    fn new(slots: Box<[Slot]>) -> Self {
        let len = slots.len();
        let base = Box::into_raw(slots).cast::<Slot>();
        let _ = base.expose_provenance();
        Self { base, len }
    }
    fn get(&self, index: u32) -> Option<&Slot> {
        let i = index as usize;
        (i < self.len).then(|| unsafe { &*self.base.add(i) })
    }
    const fn len(&self) -> usize {
        self.len
    }
    const fn is_empty(&self) -> bool {
        self.len == 0
    }
}
impl Drop for Slots {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(self.base, self.len)));
        }
    }
}
impl Pool {
    pub(crate) fn note_move(&self, from: u32, into: u32) {
        if !self
            .slot(into)
            .is_some_and(|slot| crate::calc::move_counted(slot.frame.gsn(), slot.frame.on_disk()))
        {
            return;
        }
        let (source, target) = (self.partition_of(from), self.partition_of(into));
        if let Some(count) = self.moved_in.get(target) {
            count.fetch_add(1, core::sync::atomic::Ordering::Release);
        }
        if source != target {
            self.carry_floor(source, target);
        }
    }
    fn carry_floor(&self, source: usize, target: usize) {
        let (Some(a), Some(b)) =
            (self.vouches.get(source.min(target)), self.vouches.get(source.max(target)))
        else {
            return;
        };
        let (Ok(mut first), Ok(mut second)) = (a.lock(), b.lock()) else { return };
        let (from, into) =
            if source < target { (&*first, &mut *second) } else { (&*second, &mut *first) };
        for w in 0..into.pending.len() {
            let carried = from.vouched(w);
            if let Some(floor) = into.pending.get_mut(w) {
                *floor = (*floor).min(carried);
            }
        }
    }
    pub(crate) fn vouch_from(&self, begins: &[u64]) {
        for vouch in &*self.vouches {
            if let Ok(mut v) = vouch.lock() {
                v.mark = begins.to_vec();
                v.pending = vec![u64::MAX; begins.len()];
                v.covered = vec![u64::MAX; begins.len()];
            }
        }
    }
    pub(crate) fn floors_covered(&self, of: core::ops::Range<usize>) {
        for vouch in self.vouches.get(of).into_iter().flatten() {
            if let Ok(mut v) = vouch.lock() {
                let Vouch { pending, covered, .. } = &mut *v;
                for (c, p) in covered.iter_mut().zip(pending.iter_mut()) {
                    *c = (*c).min(*p);
                    *p = u64::MAX;
                }
            }
        }
    }
    pub(crate) fn mark_clean(&self, of: core::ops::Range<usize>, captured: &[u64]) {
        for vouch in self.vouches.get(of).into_iter().flatten() {
            if let Ok(mut v) = vouch.lock() {
                v.mark.clear();
                v.mark.extend_from_slice(captured);
                v.covered.fill(u64::MAX);
            }
        }
    }
    pub(crate) fn least_vouched(&self, workers: usize) -> (Vec<u64>, bool) {
        let mut least = vec![u64::MAX; workers];
        let mut floored = false;
        for vouch in &*self.vouches {
            let Ok(v) = vouch.lock() else { return (vec![0; workers], floored) };
            floored |= v.pending.iter().chain(&v.covered).any(|&f| f != u64::MAX);
            for (w, vouched) in least.iter_mut().enumerate() {
                *vouched = (*vouched).min(v.vouched(w));
            }
        }
        (least, floored)
    }
    pub(crate) fn owe_retired(&self, from: u32, page: PageId, gsn: u64, kind: u8) {
        let partition = self.partition_of(from);
        if let Ok(mut retired) = self.retired.lock() {
            retired.push(Retired { partition, page, gsn, kind });
        }
    }
    pub(crate) fn give_back(&self, page: PageId, gsn: u64) {
        self.reissuable.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((page, gsn));
    }
    pub(crate) fn share_reissuable(&mut self, list: Reissuable) {
        self.reissuable = list;
    }
    pub(crate) fn reissuable_snapshot(&self) -> Vec<(u64, u64)> {
        self.reissuable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|&(page, gsn)| (page.get(), gsn))
            .collect()
    }
    pub(crate) fn take_reissuable(&self) -> Option<(PageId, u64)> {
        self.reissuable.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop()
    }
    pub(crate) fn owed_pages(&self) -> Vec<PageId> {
        self.retired.lock().map(|r| r.iter().map(|r| r.page).collect()).unwrap_or_default()
    }
    pub(crate) fn take_retired(&self, of: core::ops::Range<usize>) -> Vec<Retired> {
        let Ok(mut retired) = self.retired.lock() else { return Vec::new() };
        if retired.is_empty() {
            return Vec::new();
        }
        let (taken, kept) = retired.drain(..).partition(|r| of.contains(&r.partition));
        *retired = kept;
        taken
    }
    pub(crate) fn still_owed(&self, owed: impl IntoIterator<Item = Retired>) {
        if let Ok(mut retired) = self.retired.lock() {
            retired.extend(owed);
        }
    }
    pub(crate) fn captures_of(&self, index: u32) -> u64 {
        self.captures
            .get(self.partition_of(index))
            .map_or(1, |c| c.load(core::sync::atomic::Ordering::SeqCst))
    }
    pub(crate) fn capture_begins(&self, of: core::ops::Range<usize>) {
        for count in self.captures.get(of).into_iter().flatten() {
            count.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
    }
    pub(crate) fn capture_ends(&self, of: core::ops::Range<usize>) {
        for count in self.captures.get(of).into_iter().flatten() {
            count.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
    }
    pub(crate) fn moved_into(&self, of: core::ops::Range<usize>) -> u64 {
        self.moved_in.get(of).map_or(0, |counts| {
            counts.iter().map(|c| c.load(core::sync::atomic::Ordering::Acquire)).sum()
        })
    }
    pub(crate) fn frames_of(&self, of: core::ops::Range<usize>) -> core::ops::Range<usize> {
        let (frames, n) = (self.slots.len(), self.partitions.len());
        partition_start(frames, n, of.start.min(n))..partition_start(frames, n, of.end.min(n))
    }
    pub fn new(frames: usize, partitions: usize) -> Result<Self, Error> {
        Self::with_cooling_percent(frames, partitions, cooling::DEFAULT_COOLING_PERCENT)
    }
    pub fn with_cooling_percent(
        frames: usize,
        partitions: usize,
        percent: usize,
    ) -> Result<Self, Error> {
        if partitions == 0 || frames < partitions {
            return Err(Error::PoolTooSmall { frames, partitions });
        }
        let slots: Box<[Slot]> = (0..frames)
            .map(|_| Slot {
                frame: BufferFrame::new(),
                page: PageLatch::new(Node::new(PageKind::Leaf, b"", b"")),
            })
            .collect();
        let base = frames / partitions;
        let extra = frames % partitions;
        let mut next = 0_u32;
        let parts: Box<[FreeList]> = (0..partitions)
            .map(|p| {
                let n = base + usize::from(p < extra);
                let start = next;
                next += u32::try_from(n).unwrap_or(u32::MAX);
                let mut held = Vec::with_capacity(2 * n + 16);
                held.extend((start..next).rev());
                FreeList::preloaded(held)
            })
            .collect();
        let per = (frames / partitions).div_ceil(16).max(4);
        let tables: Box<[InflightTable]> =
            (0..partitions).map(|_| InflightTable::new(per)).collect();
        let cool_capacity = cooling::capacity_for(frames / partitions, percent);
        let queues: Box<[CoolingStage]> =
            (0..partitions).map(|_| CoolingStage::new(cool_capacity)).collect();
        let pool = Self {
            slots: Slots::new(slots),
            partitions: parts,
            cooling: queues,
            inflight: tables,
            moved_in: (0..partitions).map(|_| core::sync::atomic::AtomicU64::new(0)).collect(),
            captures: (0..partitions).map(|_| core::sync::atomic::AtomicU64::new(2)).collect(),
            logged: (0..frames).map(|_| core::sync::atomic::AtomicU64::new(0)).collect(),
            retired: crate::sync::Mutex::new(Vec::new()),
            reissuable: std::sync::Arc::new(crate::sync::Mutex::new(Vec::new())),
            vouches: (0..partitions).map(|_| crate::sync::Mutex::new(Vouch::default())).collect(),
        };
        drop(pool.retired.lock());
        for vouch in &*pool.vouches {
            drop(vouch.lock());
        }
        Ok(pool)
    }
    #[must_use]
    pub const fn len(&self) -> usize {
        self.slots.len()
    }
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    #[must_use]
    pub const fn partitions(&self) -> usize {
        self.partitions.len()
    }
    #[must_use]
    pub fn slot(&self, index: u32) -> Option<&Slot> {
        self.slots.get(index)
    }
    #[must_use]
    pub fn partition_for(&self, page: PageId) -> usize {
        const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
        let mixed = page.get().wrapping_mul(GOLDEN) >> 32;
        (mixed as usize) % self.partitions.len()
    }
    #[must_use]
    pub fn acquire(&self, page: PageId) -> Option<u32> {
        let start = self.partition_for(page);
        for offset in 0..self.partitions.len() {
            let p = (start + offset) % self.partitions.len();
            if let Some(index) = self.partitions[p].pop_and_claim(|index| {
                self.slots.get(index).is_some_and(|slot| slot.frame.begin_load(page).is_ok())
            }) {
                self.forget_logged(index);
                return Some(index);
            }
        }
        None
    }
    #[must_use]
    pub fn claim(&self, index: u32, page: PageId) -> bool {
        let claimed = self.slots.get(index).is_some_and(|slot| slot.frame.begin_load(page).is_ok());
        if claimed {
            self.forget_logged(index);
        }
        claimed
    }
    fn forget_logged(&self, index: u32) {
        if let Some(l) = self.logged.get(index as usize) {
            l.store(0, core::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(crate) fn logged(&self, index: u32) -> u64 {
        self.logged.get(index as usize).map_or(0, |l| l.load(core::sync::atomic::Ordering::Relaxed))
    }
    pub(crate) fn log_under(&self, index: u32, count: u64) {
        if let Some(l) = self.logged.get(index as usize) {
            l.store(count, core::sync::atomic::Ordering::Relaxed);
        }
    }
    pub fn release(&self, index: u32) -> Result<(), Error> {
        let slot = self.slots.get(index).ok_or(Error::NoSuchFrame { index })?;
        let state = slot.frame.state();
        if state != State::Free {
            return Err(Error::FrameNotFree { index, state });
        }
        self.partitions[self.partition_of(index)].push_compacting(index, |i| {
            self.slots.get(i).is_some_and(|s| s.frame.state() == State::Free)
        });
        Ok(())
    }
    pub(crate) fn partition_of(&self, index: u32) -> usize {
        partition_holding(self.slots.len(), self.partitions.len(), index as usize)
    }
    pub fn claim_read(&self, page: PageId) -> Claim {
        self.inflight[self.partition_for(page)].claim(page)
    }
    #[must_use]
    pub fn reads(&self, page: PageId) -> &InflightTable {
        &self.inflight[self.partition_for(page)]
    }
    #[must_use]
    pub fn read_holders(&self, page: PageId) -> u32 {
        self.inflight[self.partition_for(page)].holders(page)
    }
    #[must_use]
    pub fn reads_in_flight(&self) -> usize {
        self.inflight.iter().map(InflightTable::len).sum()
    }
    #[must_use]
    pub fn cooling(&self, partition: usize) -> Option<&CoolingStage> {
        self.cooling.get(partition)
    }
    #[must_use]
    pub fn cooling_for_frame(&self, index: u32) -> &CoolingStage {
        &self.cooling[self.partition_of(index)]
    }
    #[must_use]
    pub fn cooling_pages(&self) -> usize {
        self.cooling.iter().map(CoolingStage::len).sum()
    }
    #[must_use]
    pub fn free_frames_in(&self, partition: usize) -> Option<usize> {
        self.partitions.get(partition).map(FreeList::count)
    }
    #[must_use]
    pub fn highest_gsn(&self) -> u64 {
        (0..self.slots.len())
            .filter_map(|i| self.slot(u32::try_from(i).ok()?))
            .map(|slot| slot.frame.gsn())
            .max()
            .unwrap_or(0)
    }
    #[must_use]
    pub fn free_frames(&self) -> usize {
        self.partitions.iter().map(FreeList::len).sum()
    }
}
impl core::fmt::Debug for Pool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pool")
            .field("slots", &self.slots.len())
            .field("partitions", &self.partitions.len())
            .field("free", &self.free_frames())
            .field("inflight", &self.inflight.len())
            .field("reads_in_flight", &self.reads_in_flight())
            .field("cooling", &self.cooling_pages())
            .field("moved_in", &self.moved_into(0..self.partitions.len()))
            .field("retired_owed", &self.retired.lock().map_or(0, |r| r.len()))
            .field("reissuable", &self.reissuable.lock().map_or(0, |r| r.len()))
            .field("captures", &self.captures.len())
            .field("logged", &self.logged.len())
            .field("vouches", &self.vouches.len())
            .finish()
    }
}
#[inline]
pub(crate) const fn partition_start(frames: usize, n: usize, p: usize) -> usize {
    let (base, extra) = (frames / n, frames % n);
    p * base + if p < extra { p } else { extra }
}
#[inline]
pub(crate) const fn partition_holding(frames: usize, n: usize, i: usize) -> usize {
    let (base, extra) = (frames / n, frames % n);
    let wide = extra * (base + 1);
    if i < wide { i / (base + 1) } else { extra + (i - wide) / if base > 1 { base } else { 1 } }
}
