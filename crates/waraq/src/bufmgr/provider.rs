use crate::PageId;
use crate::bufmgr::cooling::{Skipped, Verdict, examine};
use crate::bufmgr::parent::Children;
use crate::bufmgr::pool::Pool;
use crate::child::ChildRef;
use crate::page::checksum;
use crate::store::channel::IoChannel;
use crate::wal::flushed::Flushed;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub rounds: u64,
    pub frames_examined: u64,
    pub pages_cooled: u64,
    pub pages_flushed: u64,
    pub pages_evicted: u64,
    pub skipped_not_hot: u64,
    pub skipped_already_cooling: u64,
    pub skipped_being_written: u64,
    pub skipped_queue_full: u64,
    pub skipped_log_behind: u64,
    pub deferred: u64,
    pub parent_unavailable: u64,
    pub skipped_latched: u64,
    pub skipped_dirty: u64,
    pub skipped_moved: u64,
    pub root_unknown: u64,
    pub unanswered: u64,
}
impl Counters {
    #[must_use]
    pub fn wrote_everything(self) -> bool {
        self.skipped_latched == 0
            && self.skipped_log_behind == 0
            && self.skipped_being_written == 0
            && self.skipped_moved == 0
            && self.unanswered == 0
    }
    #[must_use]
    pub fn touches_per_eviction(self) -> Option<f64> {
        #[expect(clippy::cast_precision_loss, reason = "a ratio for a report, not arithmetic")]
        (self.pages_evicted > 0).then(|| self.frames_examined as f64 / self.pages_evicted as f64)
    }
    fn record(&mut self, why: Skipped) {
        match why {
            Skipped::NotHot => self.skipped_not_hot += 1,
            Skipped::AlreadyCooling => self.skipped_already_cooling += 1,
            Skipped::BeingWritten => self.skipped_being_written += 1,
            Skipped::QueueFull => self.skipped_queue_full += 1,
            Skipped::LogBehind => self.skipped_log_behind += 1,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub sample: u32,
    pub evict: u32,
    pub free_target: usize,
    pub demand: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self { sample: 64, evict: 64, free_target: 1, demand: 0 }
    }
}
impl Budget {
    #[must_use]
    pub fn for_pool(frames: usize, partitions: usize) -> Self {
        let per = frames / partitions.max(1);
        Self { free_target: (per / 100).max(1), ..Self::default() }
    }
}
#[derive(Debug)]
pub struct Sampler(u64);
impl Sampler {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    #[must_use]
    pub fn index(&mut self, frames: u32) -> u32 {
        if frames == 0 {
            return 0;
        }
        u32::try_from(self.next() % u64::from(frames)).unwrap_or(0)
    }
}
pub fn run_once(
    pool: &Pool,
    children: &impl Children,
    channel: &dyn IoChannel,
    sampler: &mut Sampler,
    budget: Budget,
    flushed: Option<&Flushed>,
) -> Counters {
    let mut counters = Counters { rounds: 1, ..Counters::default() };
    cool_pass(pool, children, sampler, budget, &mut counters, flushed);
    let pending = evict_pass(pool, children, channel, budget, &mut counters, flushed);
    drain(pool, children, channel, pending, &mut counters, Reaped::Evict);
    counters
}
#[derive(Debug)]
struct Outstanding {
    frame: u32,
    page: PageId,
    gsn: u64,
}
fn under_pressure(pool: &Pool, budget: Budget) -> bool {
    (0..pool.partitions())
        .any(|p| pool.free_frames_in(p).is_some_and(|free| free < budget.free_target))
}
fn cool_pass(
    pool: &Pool,
    children: &impl Children,
    sampler: &mut Sampler,
    budget: Budget,
    counters: &mut Counters,
    flushed: Option<&Flushed>,
) {
    let frames = u32::try_from(pool.len()).unwrap_or(u32::MAX);
    if frames == 0 {
        return;
    }
    if !under_pressure(pool, budget) && budget.demand == 0 {
        return;
    }
    let mut room: usize = (0..pool.partitions())
        .filter_map(|p| pool.cooling(p))
        .map(|q| q.capacity().saturating_sub(q.len()))
        .sum();
    let mut failed = 0_u32;
    while room > 0 && failed < budget.sample {
        let index = sampler.index(frames);
        counters.frames_examined += 1;
        failed += 1;
        let Some(slot) = pool.slot(index) else { continue };
        let queue = pool.cooling_for_frame(index);
        let log_covers =
            flushed.is_none_or(|f| !slot.frame.is_dirty() || f.may_write_back(slot.frame.gsn()));
        let verdict = examine(
            slot.frame.state(),
            slot.frame.writebacks_in_flight(),
            log_covers,
            queue.has_room(),
        );
        match verdict {
            Verdict::Skip(why) => counters.record(why),
            Verdict::Cool => {
                if cool_one(pool, children, index, counters) {
                    if queue.admit(index) {
                        counters.pages_cooled += 1;
                        room -= 1;
                        failed = 0;
                    } else {
                        let _ = slot.frame.warm();
                        counters.skipped_queue_full += 1;
                    }
                }
            }
        }
    }
}
fn cool_one(pool: &Pool, children: &impl Children, index: u32, counters: &mut Counters) -> bool {
    match children.is_root(index) {
        Ok(false) => {}
        Ok(true) => {
            counters.parent_unavailable += 1;
            return false;
        }
        Err(crate::Unwind) => {
            counters.root_unknown += 1;
            return false;
        }
    }
    let Some(slot) = pool.slot(index) else { return false };
    let Ok(_victim) = slot.page.try_exclusive() else {
        counters.skipped_latched += 1;
        return false;
    };
    if slot.frame.cool().is_err() {
        counters.skipped_not_hot += 1;
        return false;
    }
    true
}
fn evict_pass(
    pool: &Pool,
    children: &impl Children,
    channel: &dyn IoChannel,
    budget: Budget,
    counters: &mut Counters,
    flushed: Option<&Flushed>,
) -> Vec<Outstanding> {
    let mut pending = Vec::new();
    let evicted_before = counters.pages_evicted;
    for partition in 0..pool.partitions() {
        let Some(queue) = pool.cooling(partition) else { continue };
        let mut submitted = 0_usize;
        for _ in 0..budget.evict {
            let free = pool.free_frames_in(partition).unwrap_or(usize::MAX);
            let freed = usize::try_from(counters.pages_evicted - evicted_before)
                .unwrap_or(usize::MAX)
                .saturating_add(pending.len());
            if free.saturating_add(submitted) >= budget.free_target && freed >= budget.demand {
                break;
            }
            let Some(index) = queue.take_oldest() else { break };
            if let Some(outstanding) = submit_one(pool, children, channel, index, counters, flushed)
            {
                pending.push(outstanding);
                submitted += 1;
            }
        }
    }
    pending
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reaped {
    Evict,
    Keep,
}
pub const DRAIN_BOUND: u32 = 1024;
fn drain(
    pool: &Pool,
    children: &impl Children,
    channel: &dyn IoChannel,
    mut pending: Vec<Outstanding>,
    counters: &mut Counters,
    then: Reaped,
) {
    let mut idle = 0;
    while !pending.is_empty() {
        let _ = channel.submit();
        let Ok(completions) = channel.poll(1) else {
            counters.unanswered += pending.len() as u64;
            return;
        };
        if completions.is_empty() {
            idle += 1;
            if idle >= DRAIN_BOUND {
                counters.unanswered += pending.len() as u64;
                return;
            }
            std::thread::yield_now();
            continue;
        }
        idle = 0;
        for completion in completions {
            let Some(at) = pending.iter().position(|o| o.page == completion.page) else { continue };
            let outstanding = pending.swap_remove(at);
            let Some(slot) = pool.slot(outstanding.frame) else { continue };
            if completion.outcome.is_err() {
                slot.frame.end_writeback(0);
                counters.skipped_being_written += 1;
                if then == Reaped::Evict {
                    give_back(pool, outstanding.frame);
                }
                continue;
            }
            slot.frame.end_writeback(outstanding.gsn);
            counters.pages_flushed += 1;
            if then == Reaped::Evict {
                evict_one(pool, children, outstanding.frame, counters);
            }
        }
    }
}
pub fn write_back_all(
    pool: &Pool,
    children: &impl Children,
    channel: &dyn IoChannel,
    flushed: Option<&Flushed>,
) -> Counters {
    write_back(pool, 0..pool.partitions(), children, channel, flushed)
}
pub fn write_back(
    pool: &Pool,
    of: core::ops::Range<usize>,
    children: &impl Children,
    channel: &dyn IoChannel,
    flushed: Option<&Flushed>,
) -> Counters {
    pass(pool, of, children, channel, flushed, None)
}
#[derive(Debug)]
pub struct Deferred {
    frame: u32,
    page: PageId,
    gsn: u64,
    bytes: crate::store::channel::PageBuf,
}
pub fn write_back_every_page(
    pool: &Pool,
    of: core::ops::Range<usize>,
    children: &impl Children,
    channel: &dyn IoChannel,
    flushed: &Flushed,
    make_durable: impl FnOnce() -> Result<(), crate::Error>,
) -> Result<Counters, crate::Error> {
    let mut deferred = Vec::new();
    let mut counters = pass(pool, of, children, channel, Some(flushed), Some(&mut deferred));
    if deferred.is_empty() {
        return Ok(counters);
    }
    if let Err(e) = make_durable() {
        unwritten(pool, deferred);
        return Err(e);
    }
    write_deferred(pool, children, channel, flushed, deferred, &mut counters);
    Ok(counters)
}
fn write_deferred(
    pool: &Pool,
    children: &impl Children,
    channel: &dyn IoChannel,
    flushed: &Flushed,
    deferred: Vec<Deferred>,
    counters: &mut Counters,
) {
    let mut written = Vec::with_capacity(deferred.len());
    for copy in deferred {
        let Some(slot) = pool.slot(copy.frame) else { continue };
        if !flushed.may_write_back(copy.gsn) {
            slot.frame.end_writeback(0);
            counters.skipped_log_behind += 1;
            continue;
        }
        if channel.push_write(copy.page, copy.bytes).is_err() {
            slot.frame.end_writeback(0);
            counters.skipped_being_written += 1;
            continue;
        }
        written.push(Outstanding { frame: copy.frame, page: copy.page, gsn: copy.gsn });
    }
    if written.is_empty() {
        return;
    }
    let before = counters.pages_flushed;
    drain(pool, children, channel, written, counters, Reaped::Keep);
    counters.deferred += counters.pages_flushed - before;
}
fn unwritten(pool: &Pool, deferred: Vec<Deferred>) {
    for copy in deferred {
        if let Some(slot) = pool.slot(copy.frame) {
            slot.frame.end_writeback(0);
        }
    }
}
fn pass(
    pool: &Pool,
    of: core::ops::Range<usize>,
    children: &impl Children,
    channel: &dyn IoChannel,
    flushed: Option<&Flushed>,
    mut defer: Option<&mut Vec<Deferred>>,
) -> Counters {
    let mut counters = Counters { rounds: 1, ..Counters::default() };
    let moves_before = pool.moved_into(of.clone());
    for index in pool.frames_of(of.clone()) {
        let Ok(index) = u32::try_from(index) else { continue };
        let Some(slot) = pool.slot(index) else { continue };
        let Some(page) = slot.frame.page_id() else { continue };
        let held = slot.page.is_held();
        let unstamped = flushed.is_some() && slot.frame.gsn() == 0;
        if !slot.frame.is_dirty() {
            if held || unstamped {
                counters.skipped_latched += 1;
            }
            continue;
        }
        if slot.frame.writebacks_in_flight() > 0 {
            counters.skipped_being_written += 1;
            continue;
        }
        if let Some(flushed) = flushed
            && defer.is_none()
            && !flushed.may_write_back(slot.frame.gsn())
        {
            counters.skipped_log_behind += 1;
            continue;
        }
        match write_out(pool, children, slot, page, index, flushed, defer.as_deref_mut(), |bytes| {
            channel.push_write(page, bytes)
        }) {
            Ok(gsn) => {
                let written = vec![Outstanding { frame: index, page, gsn }];
                drain(pool, children, channel, written, &mut counters, Reaped::Keep);
            }
            Err(skip) => skip.count(&mut counters),
        }
    }
    write_retired(pool, of.clone(), channel, flushed, &mut counters);
    if pool.moved_into(of) != moves_before {
        counters.skipped_moved += 1;
    }
    counters
}
fn write_retired(
    pool: &Pool,
    of: core::ops::Range<usize>,
    channel: &dyn IoChannel,
    flushed: Option<&Flushed>,
    counters: &mut Counters,
) {
    let owed = pool.take_retired(of);
    if owed.is_empty() {
        return;
    }
    let mut unwritten = Vec::new();
    for retired in owed {
        if flushed.is_some_and(|f| !f.may_write_back(retired.gsn)) {
            counters.skipped_log_behind += 1;
            unwritten.push(retired);
            continue;
        }
        let mut node = crate::node::Node::empty(retired.kind, &[], &[]);
        node.set_gsn(retired.gsn);
        let mut bytes = node.to_page_bytes();
        checksum::stamp(&mut bytes, retired.page);
        let written = channel
            .push_write(retired.page, Box::new(bytes))
            .and_then(|id| channel.submit().map(|_| id))
            .is_ok_and(|id| completed(channel, id));
        if written {
            counters.pages_flushed += 1;
            pool.give_back(retired.page, retired.gsn);
        } else {
            counters.skipped_being_written += 1;
            unwritten.push(retired);
        }
    }
    pool.still_owed(unwritten);
}
fn completed(channel: &dyn IoChannel, id: crate::store::channel::RequestId) -> bool {
    loop {
        let Ok(completions) = channel.poll(1) else { return false };
        if completions.is_empty() {
            return false;
        }
        if let Some(c) = completions.into_iter().find(|c| c.id == id) {
            return c.outcome.is_ok();
        }
    }
}
enum NotWritten {
    Latched,
    Moved,
    Unnamed,
    LogBehind,
    InFlight,
    Refused,
    Deferred,
}
impl NotWritten {
    fn count(self, counters: &mut Counters) {
        match self {
            Self::Latched => counters.skipped_latched += 1,
            Self::Moved | Self::Unnamed => counters.skipped_moved += 1,
            Self::LogBehind => counters.skipped_log_behind += 1,
            Self::InFlight | Self::Refused => counters.skipped_being_written += 1,
            Self::Deferred => {}
        }
    }
}
fn write_out(
    pool: &Pool,
    children: &impl Children,
    slot: &crate::bufmgr::pool::Slot,
    page: PageId,
    index: u32,
    flushed: Option<&Flushed>,
    defer: Option<&mut Vec<Deferred>>,
    push: impl FnOnce(
        crate::store::channel::PageBuf,
    ) -> Result<crate::store::channel::RequestId, crate::Error>,
) -> Result<u64, NotWritten> {
    let _ = index;
    let Ok(guard) = slot.page.try_exclusive() else { return Err(NotWritten::Latched) };
    let resident =
        matches!(slot.frame.state(), crate::frame::State::Hot | crate::frame::State::Cool);
    if !resident || slot.frame.page_id() != Some(page) {
        return Err(NotWritten::Moved);
    }
    match children.resident_frame(pool, ChildRef::evicted(page)) {
        Ok(Some(named)) if named == index => {}
        Ok(_) => {
            if slot.frame.writebacks_in_flight() == 0 && slot.frame.reclaim().is_ok() {
                drop(guard);
                let _ = pool.release(index);
            }
            return Err(NotWritten::Unnamed);
        }
        Err(_) => return Err(NotWritten::Latched),
    }
    if slot.frame.writebacks_in_flight() > 0 {
        return Err(NotWritten::InFlight);
    }
    let mut node = guard.get();
    let gsn = slot.frame.gsn();
    let behind = flushed.is_some_and(|f| !f.may_write_back(gsn));
    if behind && defer.is_none() {
        return Err(NotWritten::LogBehind);
    }
    slot.frame.begin_writeback();
    drop(guard);
    node.set_gsn(gsn);
    let mut bytes = node.to_page_bytes();
    checksum::stamp(&mut bytes, page);
    if let (true, Some(defer)) = (behind, defer) {
        defer.push(Deferred { frame: index, page, gsn, bytes: Box::new(bytes) });
        return Err(NotWritten::Deferred);
    }
    let pushed = push(Box::new(bytes));
    if pushed.is_err() {
        slot.frame.end_writeback(0);
        return Err(NotWritten::Refused);
    }
    Ok(gsn)
}
fn submit_one(
    pool: &Pool,
    children: &impl Children,
    channel: &dyn IoChannel,
    index: u32,
    counters: &mut Counters,
    flushed: Option<&Flushed>,
) -> Option<Outstanding> {
    let slot = pool.slot(index)?;
    if slot.frame.state() != crate::frame::State::Cool {
        counters.skipped_not_hot += 1;
        return None;
    }
    if slot.frame.writebacks_in_flight() > 0 {
        counters.skipped_being_written += 1;
        give_back(pool, index);
        return None;
    }
    let Some(page) = slot.frame.page_id() else {
        counters.skipped_not_hot += 1;
        return None;
    };
    if pool.read_holders(page) > 0 {
        counters.skipped_being_written += 1;
        give_back(pool, index);
        return None;
    }
    if slot.frame.is_dirty() {
        if let Some(flushed) = flushed
            && !flushed.may_write_back(slot.frame.gsn())
        {
            counters.skipped_log_behind += 1;
            give_back(pool, index);
            return None;
        }
        return match write_out(pool, children, slot, page, index, flushed, None, |bytes| {
            channel.push_write(page, bytes)
        }) {
            Ok(gsn) => Some(Outstanding { frame: index, page, gsn }),
            Err(skip) => {
                skip.count(counters);
                give_back(pool, index);
                None
            }
        };
    }
    evict_one(pool, children, index, counters);
    None
}
fn give_back(pool: &Pool, index: u32) {
    if let Some(slot) = pool.slot(index) {
        let _ = slot.frame.warm();
    }
}
fn evict_one(pool: &Pool, children: &impl Children, index: u32, counters: &mut Counters) {
    let Some(slot) = pool.slot(index) else {
        return;
    };
    let Some(page) = slot.frame.page_id() else {
        counters.skipped_not_hot += 1;
        return;
    };
    let Ok(guard) = slot.page.try_exclusive() else {
        counters.skipped_latched += 1;
        give_back(pool, index);
        return;
    };
    if slot.frame.state() != crate::frame::State::Cool || slot.frame.page_id() != Some(page) {
        counters.skipped_not_hot += 1;
        return;
    }
    if slot.frame.is_dirty() {
        counters.skipped_dirty += 1;
        give_back(pool, index);
        return;
    }
    if slot.frame.writebacks_in_flight() > 0 || pool.read_holders(page) > 0 {
        counters.skipped_being_written += 1;
        give_back(pool, index);
        return;
    }
    let Ok(()) = slot.frame.evict() else {
        counters.skipped_not_hot += 1;
        return;
    };
    children.forget(page, index);
    drop(guard);
    if pool.release(index).is_ok() {
        counters.pages_evicted += 1;
    }
}
