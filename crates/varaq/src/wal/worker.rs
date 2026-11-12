use crate::Error;
use crate::sync::Mutex;
use crate::wal::flushed::Flushed;
use crate::wal::gsn::Clock;
use crate::wal::record::{Header, Kind};
use crate::wal::ring::Ring;
use core::cell::Cell;
const APPEND_BOUND: u32 = 2;
pub(crate) const COMPENSATION_BYTES: u64 =
    (crate::wal::record::HEADER_BYTES + crate::wal::record::payload::IMAGE_MAX_BYTES) as u64;
pub(crate) const SMO_RECORD_BYTES: u64 = (crate::wal::record::HEADER_BYTES
    + crate::wal::record::payload::smo_max_len(crate::wal::record::payload::SMO_MAX_PAGES))
    as u64;
pub(crate) const CLOSING_BYTES: u64 =
    (crate::wal::record::HEADER_BYTES + 1) as u64 + SMO_RECORD_BYTES;
pub(crate) const fn undo_reserve_with(entries: u64, compensation: u64) -> u64 {
    entries * compensation + CLOSING_BYTES + SMO_RECORD_BYTES
}
pub(crate) const fn change_record_bound(undo: u64, unit: u64) -> u64 {
    unit + undo
        + (crate::wal::record::payload::SMO_MAX_PAGES
            * crate::wal::record::payload::VALUE_ENTRY_PREFIX_BYTES) as u64
}
#[must_use]
pub const fn compensation_for(line: usize) -> u64 {
    if line == usize::MAX {
        COMPENSATION_BYTES
    } else {
        (crate::wal::record::HEADER_BYTES + crate::wal::record::payload::smo_max_len(3)) as u64
    }
}
#[must_use]
pub const fn ring_floor() -> u64 {
    let separating = compensation_for(0);
    let unit = if separating > COMPENSATION_BYTES { separating } else { COMPENSATION_BYTES };
    let undo = (crate::wal::record::payload::UNDO_PREFIX_BYTES + 2 * u16::MAX as usize) as u64;
    change_record_bound(undo, unit) + SMO_RECORD_BYTES + undo_reserve_with(1, unit)
}
thread_local! {
    static RECORD : core::cell::Cell < Vec < u8 >> = const {
    core::cell::Cell::new(Vec::new()) };
}
pub(crate) struct LentRecord(pub(crate) Vec<u8>);
impl LentRecord {
    pub(crate) fn take() -> Self {
        let mut body = RECORD.with(core::cell::Cell::take);
        body.clear();
        Self(body)
    }
}
impl Drop for LentRecord {
    fn drop(&mut self) {
        let body = core::mem::take(&mut self.0);
        let _ = RECORD.try_with(|cell| cell.set(body));
    }
}
thread_local! {
    static WORKER : Cell < Option < u16 >> = const { Cell::new(None) };
}
#[must_use]
pub fn current() -> Option<u16> {
    WORKER.with(Cell::get)
}
#[must_use = "the binding lasts only as long as the guard"]
pub fn bind(worker: u16) -> Bound {
    let previous = WORKER.with(|slot| slot.replace(Some(worker)));
    Bound { previous }
}
#[derive(Debug)]
pub struct Bound {
    previous: Option<u16>,
}
impl Drop for Bound {
    fn drop(&mut self) {
        WORKER.with(|slot| slot.set(self.previous));
    }
}
#[derive(Debug)]
pub struct Slot {
    pub clock: Clock,
    ring: Mutex<Ring>,
    queue: Mutex<crate::wal::driver::Queue>,
    deps: Mutex<Vec<(u32, u64)>>,
    flushing: Mutex<FlushBuffers>,
    live_from: core::sync::atomic::AtomicU64,
    in_txn: core::sync::atomic::AtomicBool,
    collecting: crate::wal::collecting::Collecting,
    undo_entries: core::sync::atomic::AtomicU64,
    durable: core::sync::atomic::AtomicU64,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Written {
    pub upto: u64,
    pub gsn: u64,
}
#[derive(Debug, Default)]
struct FlushBuffers {
    spans: [Vec<u8>; 2],
    page: Vec<u8>,
    written: Written,
}
impl FlushBuffers {
    fn copy(&mut self, first: &[u8], second: &[u8]) {
        for (span, from) in self.spans.iter_mut().zip([first, second]) {
            span.clear();
            span.extend_from_slice(from);
        }
    }
}
impl Slot {
    pub fn with_capacity(capacity: usize) -> Result<Self, Error> {
        Self::staged(capacity, capacity)
    }
    pub fn staged(memory: usize, device: usize) -> Result<Self, Error> {
        Ok(Self {
            clock: Clock::new(),
            ring: Mutex::new(Ring::staged(memory, device)?),
            queue: Mutex::new(crate::wal::driver::Queue::new()),
            deps: Mutex::new(Vec::new()),
            flushing: Mutex::new(FlushBuffers::default()),
            live_from: core::sync::atomic::AtomicU64::new(u64::MAX),
            in_txn: core::sync::atomic::AtomicBool::new(false),
            collecting: crate::wal::collecting::Collecting::new(),
            undo_entries: core::sync::atomic::AtomicU64::new(0),
            durable: core::sync::atomic::AtomicU64::new(0),
        })
    }
}
pub struct Wal {
    compensation: core::sync::atomic::AtomicU64,
    slots: Box<[Slot]>,
    capacity: usize,
    memory: usize,
    flushed: Flushed,
    barrier: crate::wal::barrier::SharedBarrier,
    device_writes: crate::stats::Stat,
    records: crate::stats::Stat,
    chain_slow: crate::stats::Stat,
    device: Option<Box<dyn crate::store::PageStore>>,
    base: u64,
    ceiling: std::sync::OnceLock<std::sync::Arc<crate::catalog::Keeper>>,
    on_stop: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
    data: std::sync::OnceLock<std::sync::Arc<crate::tree::blob::DataBarrier>>,
}
impl core::fmt::Debug for Wal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Wal")
            .field("workers", &self.slots.len())
            .field("capacity", &self.capacity)
            .field("on_device", &self.device.is_some())
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}
impl Slot {
    pub fn ring_ref(&self) -> Result<crate::sync::MutexGuard<'_, Ring>, Error> {
        self.ring.lock().map_err(|_| Error::Corrupt)
    }
    pub fn ring_mut(&self) -> Result<crate::sync::MutexGuard<'_, Ring>, Error> {
        self.ring.lock().map_err(|_| Error::Corrupt)
    }
    #[must_use]
    pub fn durable(&self) -> u64 {
        self.durable.load(core::sync::atomic::Ordering::Acquire)
    }
    pub fn queue_mut(
        &self,
    ) -> Result<crate::sync::MutexGuard<'_, crate::wal::driver::Queue>, Error> {
        self.queue.lock().map_err(|_| Error::Corrupt)
    }
}
impl Wal {
    #[must_use]
    pub fn queued_upto(&self, worker: usize, txn: u64) -> Option<u64> {
        let queue = self.slots.get(worker)?.queue_mut().ok()?;
        queue.iter().find(|e| e.txn == txn).map(|e| e.upto)
    }
    pub fn queued(
        &self,
        worker: usize,
        txn: u64,
    ) -> Result<Option<crate::wal::driver::Precommitted>, Error> {
        let slot = self
            .slots
            .get(worker)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker).unwrap_or(u32::MAX) })?;
        Ok(slot.queue_mut()?.iter().find(|e| e.txn == txn).cloned())
    }
    pub fn first_covered(&self, acked: impl Fn(u16, u64) -> bool) -> Result<Option<u64>, Error> {
        let slot = self.mine()?;
        let queue = slot.queue_mut()?;
        let Some(front) = queue.iter().next() else { return Ok(None) };
        let covered =
            self.releasable(front)? && front.waits.iter().all(|&(on, tag)| acked(on, tag));
        Ok(covered.then_some(front.txn))
    }
    pub fn queued_all(
        &self,
        worker: usize,
    ) -> Result<Vec<crate::wal::driver::Precommitted>, Error> {
        let slot = self
            .slots
            .get(worker)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker).unwrap_or(u32::MAX) })?;
        Ok(slot.queue_mut()?.iter().cloned().collect())
    }
    #[must_use]
    pub fn awaiting_ack(&self, worker: usize) -> bool {
        self.slots.get(worker).is_some_and(|s| s.queue_mut().is_ok_and(|q| !q.is_empty()))
    }
    #[must_use]
    pub fn slots_ref(&self) -> &[Slot] {
        &self.slots
    }
    pub fn with_capacity(workers: usize, capacity: usize) -> Result<Self, Error> {
        Self::staged(workers, capacity, capacity)
    }
    pub fn staged(workers: usize, memory: usize, device: usize) -> Result<Self, Error> {
        assert!(workers > 0, "a log needs at least one worker");
        let most = usize::from(u16::MAX) + 1;
        if workers > most {
            return Err(Error::TooManyWorkers { requested: workers, most });
        }
        let mut slots = Vec::with_capacity(workers);
        for _ in 0..workers {
            slots.push(Slot::staged(memory, device)?);
        }
        Ok(Self {
            slots: slots.into_boxed_slice(),
            capacity: device,
            memory,
            flushed: Flushed::new(workers),
            barrier: crate::wal::barrier::SharedBarrier::new(),
            device_writes: crate::stats::Stat::default(),
            records: crate::stats::Stat::default(),
            chain_slow: crate::stats::Stat::default(),
            compensation: core::sync::atomic::AtomicU64::new(compensation_for(
                crate::node::SEPARATE_ABOVE,
            )),
            device: None,
            base: 1,
            ceiling: std::sync::OnceLock::new(),
            on_stop: std::sync::OnceLock::new(),
            data: std::sync::OnceLock::new(),
        })
    }
    pub fn keep_below(&self, catalog: std::sync::Arc<crate::catalog::Keeper>) {
        let _ = self.ceiling.set(catalog);
    }
    pub fn on_device(
        mut self,
        device: Box<dyn crate::store::PageStore>,
        base: u64,
    ) -> Result<Self, Error> {
        if base == 0 {
            return Err(Error::Corrupt);
        }
        let pages = self
            .ring_pages()
            .checked_mul(self.slots.len() as u64)
            .ok_or(Error::RingTooLarge { bytes: self.capacity as u64 })?;
        if device.allocate_run(pages)?.get() != base {
            return Err(Error::Corrupt);
        }
        self.attach(device, base)?;
        Ok(self)
    }
    fn attach(&mut self, device: Box<dyn crate::store::PageStore>, base: u64) -> Result<(), Error> {
        for slot in &self.slots {
            slot.ring_mut()?.rewrites_in(crate::PAGE_SIZE);
        }
        self.device = Some(device);
        self.base = base;
        Ok(())
    }
    #[must_use]
    pub fn device(&self) -> Option<&dyn crate::store::PageStore> {
        self.device.as_deref()
    }
    #[must_use]
    pub fn ring_device_bytes(&self) -> u64 {
        self.ring_pages() * crate::PAGE_SIZE as u64
    }
    pub fn read_ring(&self, worker: usize, at: u64, into: &mut [u8]) -> Result<(), Error> {
        let device = self.device().ok_or(Error::Corrupt)?;
        if worker >= self.slots.len()
            || at.checked_add(into.len() as u64).is_none_or(|end| end > self.ring_device_bytes())
        {
            return Err(Error::Corrupt);
        }
        let page = crate::PAGE_SIZE as u64;
        let first = self.base + worker as u64 * self.ring_pages();
        let mut done = 0usize;
        let mut buf = [0u8; crate::PAGE_SIZE];
        while done < into.len() {
            let offset = at + done as u64;
            let pid = crate::PageId::new(first + offset / page)?;
            let within = usize::try_from(offset % page).map_err(|_| Error::Corrupt)?;
            let n = (crate::PAGE_SIZE - within).min(into.len() - done);
            if within == 0 && n == crate::PAGE_SIZE {
                let whole = (into.len() - done) / crate::PAGE_SIZE * crate::PAGE_SIZE;
                device.read_raw_run(pid, &mut into[done..done + whole])?;
                done += whole;
            } else {
                device.read_raw(pid, &mut buf)?;
                into[done..done + n].copy_from_slice(&buf[within..within + n]);
                done += n;
            }
        }
        Ok(())
    }
    pub fn write_back(&self, worker: usize, at: u64, bytes: &[u8]) -> Result<(), Error> {
        let device = self.device().ok_or(Error::Corrupt)?;
        let page = crate::PAGE_SIZE as u64;
        if worker >= self.slots.len()
            || !at.is_multiple_of(page)
            || !bytes.len().is_multiple_of(crate::PAGE_SIZE)
            || at.checked_add(bytes.len() as u64).is_none_or(|end| end > self.ring_device_bytes())
        {
            return Err(Error::Corrupt);
        }
        let first = self.base + worker as u64 * self.ring_pages() + at / page;
        device.write_run(crate::PageId::new(first)?, bytes)
    }
    pub fn reopen(
        mut self,
        device: Box<dyn crate::store::PageStore>,
        base: u64,
    ) -> Result<Self, Error> {
        if base == 0 {
            return Err(Error::Corrupt);
        }
        let pages = self
            .ring_pages()
            .checked_mul(self.slots.len() as u64)
            .ok_or(Error::RingTooLarge { bytes: self.capacity as u64 })?;
        let end =
            base.checked_add(pages).ok_or(Error::RingTooLarge { bytes: self.capacity as u64 })?;
        let last = crate::PageId::new(end - 1).map_err(|_| Error::Corrupt)?;
        let mut page = vec![0u8; crate::PAGE_SIZE];
        device.read_raw(last, &mut page).map_err(|_| Error::Corrupt)?;
        self.attach(device, base)?;
        Ok(self)
    }
    pub fn flush(&self, idle: bool) -> Result<bool, Error> {
        let (wrote, written) = self.write_out(idle)?;
        let worker = current().ok_or(Error::NoWorkerBound)?;
        if written.upto <= self.mine()?.durable() {
            return Ok(wrote);
        }
        self.sync()?;
        self.mark_durable(worker, written);
        Ok(true)
    }
    pub fn write_out(&self, idle: bool) -> Result<(bool, Written), Error> {
        if self.is_poisoned() {
            return Err(Error::LogPoisoned);
        }
        let worker = current().ok_or(Error::NoWorkerBound)?;
        let slot = self.mine()?;
        let mut serial = slot.flushing.lock().map_err(|_| Error::Corrupt)?;
        let buffers = &mut *serial;
        let (from, upto, covered) = {
            let ring = slot.ring_ref()?;
            let pending = ring.pending();
            if pending == 0 {
                return Ok((false, buffers.written));
            }
            if pending < self.flush_unit() && !idle {
                return Ok((false, buffers.written));
            }
            let from = ring.flushed() - ring.flushed() % crate::PAGE_SIZE as u64;
            if self.device.is_some() {
                let (first, second) = ring.spans_from(from);
                buffers.copy(first, second);
            }
            (from, ring.written(), slot.clock.now().max(ring.last_gsn()))
        };
        if let Some(data) = self.data.get() {
            data.cover(usize::from(worker))?;
        }
        if let Some(device) = self.device.as_ref() {
            if let Some(catalog) = self.ceiling.get()
                && covered >= catalog.ceiling()
                && let Err(e) = catalog.raise(covered)
            {
                self.poison();
                return Err(e);
            }
            self.write_spans(device.as_ref(), worker, from, buffers)?;
        }
        slot.ring_mut()?.mark_flushed(upto)?;
        buffers.written = Written { upto, gsn: covered };
        Ok((true, buffers.written))
    }
    pub fn sync(&self) -> Result<(), Error> {
        if self.is_poisoned() {
            return Err(Error::LogPoisoned);
        }
        match self.device.as_ref() {
            Some(device) => self.barrier.sync(device.as_ref()),
            None => Ok(()),
        }
    }
    pub fn make_every_log_durable(&self) -> Result<(), Error> {
        for worker in 0..self.workers() {
            let _bound = bind(u16::try_from(worker).map_err(|_| Error::Corrupt)?);
            self.flush(true)?;
            let _ = self.barrier_transaction()?;
        }
        Ok(())
    }
    pub fn mark_durable(&self, worker: u16, written: Written) {
        let slot = &self.slots[usize::from(worker)];
        slot.durable.fetch_max(written.upto, core::sync::atomic::Ordering::AcqRel);
        let _ = self.flushed.publish(usize::from(worker), written.gsn);
    }
    #[must_use]
    pub fn barrier(&self) -> &crate::wal::barrier::SharedBarrier {
        &self.barrier
    }
    #[must_use]
    pub fn device_writes(&self) -> u64 {
        self.device_writes.get()
    }
    #[must_use]
    pub fn chain_slow_paths(&self) -> u64 {
        self.chain_slow.get()
    }
    #[must_use]
    pub fn records(&self) -> u64 {
        self.records.get()
    }
    fn write_spans(
        &self,
        device: &dyn crate::store::PageStore,
        worker: u16,
        from: u64,
        buffers: &mut FlushBuffers,
    ) -> Result<(), Error> {
        let page_size = crate::PAGE_SIZE as u64;
        let FlushBuffers { spans, page: zeroed, .. } = buffers;
        zeroed.resize(crate::PAGE_SIZE, 0);
        for span in spans.iter_mut() {
            if span.len() > crate::PAGE_SIZE && span.len() % crate::PAGE_SIZE != 0 {
                span.resize(span.len().next_multiple_of(crate::PAGE_SIZE), 0);
            }
        }
        let mut at = from;
        for span in spans.iter() {
            let mut written = 0usize;
            while written < span.len() {
                let page_index = at / page_size;
                let offset = usize::try_from(at % page_size).unwrap_or(0);
                let ring_pages = self.ring_pages();
                let whole = if offset == 0 { (span.len() - written) / crate::PAGE_SIZE } else { 0 };
                let to_wrap = ring_pages - page_index % ring_pages;
                let run = whole.min(usize::try_from(to_wrap).unwrap_or(usize::MAX));
                if run > 1 {
                    let first =
                        self.base + u64::from(worker) * ring_pages + page_index % ring_pages;
                    let bytes = &span[written..written + run * crate::PAGE_SIZE];
                    device.write_run(crate::PageId::new(first)?, bytes)?;
                    self.device_writes.add(1);
                    written += bytes.len();
                    at += bytes.len() as u64;
                    continue;
                }
                let take = span.len().min(written + crate::PAGE_SIZE - offset) - written;
                let buf: &[u8] = if offset == 0 && take == crate::PAGE_SIZE {
                    &span[written..written + take]
                } else {
                    zeroed.fill(0);
                    zeroed[offset..offset + take].copy_from_slice(&span[written..written + take]);
                    zeroed
                };
                let ring_pages = self.ring_pages();
                let page = self.base + u64::from(worker) * ring_pages + page_index % ring_pages;
                let pid = crate::PageId::new(page)?;
                device.write(pid, buf)?;
                self.device_writes.add(1);
                written += take;
                at += take as u64;
            }
        }
        Ok(())
    }
    pub fn poison(&self) {
        self.flushed.stop();
        if let Some(tell) = self.on_stop.get() {
            tell();
        }
    }
    pub fn on_stop(&self, tell: Box<dyn Fn() + Send + Sync>) -> bool {
        self.on_stop.set(tell).is_ok()
    }
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.flushed.is_stopped()
    }
    #[must_use]
    pub fn ring_capacity(&self) -> usize {
        self.capacity
    }
    #[must_use]
    pub fn ring_memory(&self) -> usize {
        self.memory
    }
    fn ring_pages(&self) -> u64 {
        (self.capacity as u64).div_ceil(crate::PAGE_SIZE as u64).max(1)
    }
    #[must_use]
    pub fn workers(&self) -> usize {
        self.slots.len()
    }
    #[must_use]
    pub fn data_barrier_for(
        &self,
        store: &std::sync::Arc<dyn crate::store::PageStore>,
    ) -> Option<std::sync::Arc<crate::tree::blob::DataBarrier>> {
        let barrier = self.data.get_or_init(|| {
            std::sync::Arc::new(crate::tree::blob::DataBarrier::new(
                std::sync::Arc::clone(store),
                self.slots.len(),
            ))
        });
        barrier.is_over(store).then(|| std::sync::Arc::clone(barrier))
    }
    #[must_use]
    pub fn flushed(&self) -> &Flushed {
        &self.flushed
    }
    pub fn note_prior(&self, prior: u64, writer: u32) {
        self.chain_onto(prior, writer);
    }
    pub(crate) fn chain_onto(&self, gsn: u64, writer: u32) {
        let Some(me) = current() else { return };
        if writer == crate::frame::NO_WRITER || writer == u32::from(me) {
            return;
        }
        if gsn <= self.flushed.global_min() {
            return;
        }
        self.chain_slow.add(1);
        let Ok(slot) = self.mine() else { return };
        let mut found: Vec<(u32, u64)> = Vec::new();
        match self.slots_ref().get(writer as usize) {
            Some(theirs) => {
                if let Ok(deps) = theirs.deps.lock() {
                    found.extend(deps.iter().copied().filter(|&(w, _)| w != u32::from(me)));
                }
                if gsn > self.flushed.of(writer as usize) {
                    found.push((writer, gsn));
                }
            }
            None => found.push((writer, gsn)),
        }
        let Ok(mut deps) = slot.deps.lock() else { return };
        for (w, g) in found {
            match deps.iter_mut().find(|(have, _)| *have == w) {
                Some((_, at)) => *at = (*at).max(g),
                None => deps.push((w, g)),
            }
        }
    }
    pub(crate) fn outstanding(&self) -> Result<Vec<(u32, u64)>, Error> {
        let slot = self.mine()?;
        let mut deps = slot.deps.lock().map_err(|_| Error::Corrupt)?;
        deps.retain(|&(w, g)| {
            (w as usize) >= self.slots_ref().len() || self.flushed.of(w as usize) < g
        });
        Ok(deps.clone())
    }
    pub(crate) fn resume_clocks(&self, gsn: u64) {
        for (worker, slot) in self.slots_ref().iter().enumerate() {
            let _ = slot.clock.observe(gsn);
            let _ = self.flushed.publish(worker, gsn);
        }
    }
    pub(crate) fn begin_txn(&self, worker: usize) {
        if let Some(slot) = self.slots_ref().get(worker) {
            slot.in_txn.store(true, core::sync::atomic::Ordering::SeqCst);
            slot.undo_entries.store(0, core::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(crate) fn collecting(&self, worker: usize) -> Option<crate::wal::collecting::Guard<'_>> {
        self.slots_ref().get(worker).map(|slot| slot.collecting.enter())
    }
    #[must_use]
    pub fn is_collecting(&self, worker: usize) -> bool {
        self.slots.get(worker).is_some_and(|s| s.collecting.any())
    }
    pub(crate) fn end_txn(&self, worker: usize) {
        if let Some(slot) = self.slots_ref().get(worker) {
            slot.in_txn.store(false, core::sync::atomic::Ordering::SeqCst);
            slot.live_from.store(u64::MAX, core::sync::atomic::Ordering::SeqCst);
            slot.undo_entries.store(0, core::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(crate) fn positions(&self) -> Result<Vec<u64>, Error> {
        self.slots_ref().iter().map(|s| Ok(s.ring_ref()?.written())).collect()
    }
    pub(crate) fn live_from(&self, worker: usize) -> Option<u64> {
        let at = self.slots_ref().get(worker)?.live_from.load(core::sync::atomic::Ordering::SeqCst);
        (at != u64::MAX).then_some(at)
    }
    pub(crate) fn retain_everything(&self) -> Result<(), Error> {
        for slot in self.slots_ref() {
            slot.ring_mut()?.retain_everything();
        }
        Ok(())
    }
    pub(crate) fn release_before(&self, worker: usize, at: u64) -> Result<(), Error> {
        self.slots_ref().get(worker).ok_or(Error::Corrupt)?.ring_mut()?.release_before(at)
    }
    #[must_use]
    pub fn ring_position(&self, worker: usize) -> Option<(u64, u64)> {
        self.slots_ref()
            .get(worker)
            .and_then(|s| s.ring_ref().ok().map(|r| (r.written(), r.capacity() as u64)))
    }
    #[must_use]
    pub fn ring_retained(&self, worker: usize) -> Option<u64> {
        self.slots_ref().get(worker).and_then(|s| s.ring_ref().ok().and_then(|r| r.retained_from()))
    }
    pub(crate) fn room_for_optional_smo(&self) -> Result<bool, Error> {
        let slot = self.mine()?;
        let entries = slot.undo_entries.load(core::sync::atomic::Ordering::Relaxed);
        let ring = slot.ring_ref()?;
        if !ring.retains() {
            return Ok(true);
        }
        Ok(ring.free() as u64 >= SMO_RECORD_BYTES + undo_reserve_with(entries, self.compensation()))
    }
    #[must_use]
    pub fn compensation(&self) -> u64 {
        self.compensation.load(core::sync::atomic::Ordering::Relaxed)
    }
    pub fn separate_above(&self, line: usize) {
        self.compensation.store(compensation_for(line), core::sync::atomic::Ordering::Relaxed);
    }
    pub(crate) fn raise_clocks(&self, gsn: u64) {
        for slot in self.slots_ref() {
            let _ = slot.clock.observe(gsn);
        }
    }
    pub(crate) fn resume_ring(
        &self,
        worker: usize,
        at: u64,
        last_gsn: u64,
        begin: u64,
    ) -> Result<(), Error> {
        let slot = self.slots_ref().get(worker).ok_or(Error::Corrupt)?;
        let read = |offset: u64, into: &mut [u8]| self.read_ring(worker, offset, into);
        slot.ring_mut()?.resume(&read, at, last_gsn, begin)?;
        slot.durable.fetch_max(at, core::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
    pub fn mine(&self) -> Result<&Slot, Error> {
        let worker = current().ok_or(Error::NoWorkerBound)?;
        self.slots.get(usize::from(worker)).ok_or(Error::NoSuchFrame { index: u32::from(worker) })
    }
    pub fn modified(&self, page: u64, page_gsn: u64, image: &[u8]) -> Result<u64, Error> {
        self.modified_as(page, page_gsn, crate::wal::record::payload::Change::Image(image), None)?
            .ok_or(Error::Corrupt)
    }
    pub fn modified_as(
        &self,
        page: u64,
        page_gsn: u64,
        change: crate::wal::record::payload::Change<'_>,
        still: Option<&dyn Fn() -> bool>,
    ) -> Result<Option<u64>, Error> {
        use crate::wal::record::payload::{Change, read_image, read_op};
        let kind = match change {
            Change::Image(image) => {
                read_image(image)?;
                Kind::Page
            }
            Change::Op(op) => {
                read_op(op)?;
                if page_gsn == 0 {
                    return Err(Error::Corrupt);
                }
                Kind::PageOp
            }
            Change::Run(_) | Change::Retire(_) => return Err(Error::Corrupt),
        };
        let image = change.bytes();
        let slot = self.mine()?;
        let worker = current().ok_or(Error::NoWorkerBound)?;
        let header = Header {
            kind,
            worker,
            gsn: 0,
            page,
            txn: 0,
            len: u32::try_from(image.len()).map_err(|_| Error::Corrupt)?,
            prev: page_gsn,
            lap: 0,
        };
        let _ = slot;
        let Some(appended) = self.append_checked(page_gsn, header, image, 0, still)? else {
            return Ok(None);
        };
        Ok(Some(appended.0))
    }
    pub fn reserve_change(
        &self,
        undo: &crate::wal::record::payload::UndoPart<'_>,
    ) -> Result<(), Error> {
        let slot = self.mine()?;
        let undo = crate::wal::record::payload::undo_part_len(undo)? as u64;
        let entries = slot.undo_entries.load(core::sync::atomic::Ordering::Relaxed);
        let unit = self.compensation();
        let record = change_record_bound(undo, unit);
        let ring = slot.ring_ref()?;
        let (capacity, free) = (ring.capacity() as u64, ring.free() as u64);
        let needed = if ring.retains() {
            record + SMO_RECORD_BYTES + undo_reserve_with(entries + 1, unit)
        } else {
            record
        };
        if needed > capacity || (ring.retains() && free < needed) {
            return Err(Error::LogFull {
                capacity: ring.capacity(),
                needed: usize::try_from(needed).unwrap_or(usize::MAX),
                free: ring.free(),
            });
        }
        Ok(())
    }
    pub fn modified_many(
        &self,
        pages: &[(u64, u64, &[u8])],
        new_root: u64,
        kind: Kind,
    ) -> Result<u64, Error> {
        use crate::wal::record::payload::Change;
        let mut held = [(0, 0, Change::Image(&[])); crate::wal::record::payload::SMO_MAX_PAGES];
        if pages.len() > held.len() {
            return Err(Error::Corrupt);
        }
        for (to, &(page, prev, image)) in held.iter_mut().zip(pages) {
            *to = (page, prev, Change::Image(image));
        }
        self.modified_many_as(&held[..pages.len()], new_root, kind, None, None)?
            .ok_or(Error::Corrupt)
    }
    pub fn modified_many_as(
        &self,
        pages: &[(u64, u64, crate::wal::record::payload::Change<'_>)],
        new_root: u64,
        kind: Kind,
        still: Option<&dyn Fn() -> bool>,
        undo: Option<(u64, &crate::wal::record::payload::UndoPart<'_>)>,
    ) -> Result<Option<u64>, Error> {
        use crate::wal::record::payload;
        match (kind, undo.is_some()) {
            (Kind::Smo, false) => {}
            (Kind::ValueWrite, false) | (Kind::Change, true) if new_root == 0 => {}
            _ => return Err(Error::Corrupt),
        }
        let slot = self.mine()?;
        let worker = current().ok_or(Error::NoWorkerBound)?;
        let highest =
            pages.iter().filter(|e| !e.2.is_run()).map(|&(_, prev, _)| prev).max().unwrap_or(0);
        if pages.len() > payload::SMO_MAX_PAGES {
            return Err(Error::Corrupt);
        }
        let mut lent = LentRecord::take();
        let body = &mut lent.0;
        if kind == Kind::Smo {
            let mut held =
                [payload::SmoEntry { page: 0, prev: 0, image: &[] }; payload::SMO_MAX_PAGES];
            for (entry, &(page, prev, change)) in held.iter_mut().zip(pages) {
                let payload::Change::Image(image) = change else { return Err(Error::Corrupt) };
                *entry = payload::SmoEntry { page, prev, image };
            }
            let entries = &held[..pages.len()];
            body.resize(payload::smo_len(entries), 0);
            payload::write_smo(entries, body)?;
        } else {
            let mut held =
                [payload::ValueEntry { page: 0, prev: 0, change: payload::Change::Image(&[]) };
                    payload::SMO_MAX_PAGES];
            for (entry, &(page, prev, change)) in held.iter_mut().zip(pages) {
                *entry = payload::ValueEntry { page, prev, change };
            }
            let entries = &held[..pages.len()];
            if let Some((_, part)) = undo {
                body.resize(payload::change_len(part, entries)?, 0);
                payload::write_change(part, entries, body)?;
            } else {
                body.resize(payload::value_write_len(entries), 0);
                payload::write_value_write(entries, body)?;
            }
        }
        let header = Header {
            kind,
            worker,
            gsn: 0,
            page: new_root,
            txn: undo.map_or(0, |(txn, _)| txn),
            len: u32::try_from(body.len()).map_err(|_| Error::Corrupt)?,
            prev: 0,
            lap: 0,
        };
        let Some(appended) = self.append_checked(highest, header, body, 0, still)? else {
            return Ok(None);
        };
        if undo.is_some() {
            let entries = slot.undo_entries.load(core::sync::atomic::Ordering::Relaxed);
            slot.undo_entries.store(entries + 1, core::sync::atomic::Ordering::Relaxed);
        }
        Ok(Some(appended.0))
    }
    pub(crate) fn append_bounded(
        &self,
        stamp_from: u64,
        header: Header,
        payload: &[u8],
    ) -> Result<(u64, u64), Error> {
        self.append_reserving(stamp_from, header, payload, 0)
    }
    pub(crate) fn append_reserving(
        &self,
        stamp_from: u64,
        header: Header,
        payload: &[u8],
        headroom: u64,
    ) -> Result<(u64, u64), Error> {
        self.append_checked(stamp_from, header, payload, headroom, None)?.ok_or(Error::Corrupt)
    }
    pub(crate) fn append_checked(
        &self,
        stamp_from: u64,
        mut header: Header,
        payload: &[u8],
        headroom: u64,
        still: Option<&dyn Fn() -> bool>,
    ) -> Result<Option<(u64, u64)>, Error> {
        let slot = self.mine()?;
        let headroom = usize::try_from(headroom).map_err(|_| Error::Corrupt)?;
        let mut last_full = None;
        for _ in 0..APPEND_BOUND {
            let outcome = {
                let mut ring = slot.ring_mut()?;
                if still.is_some_and(|still| !still()) {
                    return Ok(None);
                }
                let len = header.total_len();
                if headroom > 0 && ring.retains() && ring.free() < len + headroom {
                    Err(Error::LogFull {
                        capacity: ring.capacity(),
                        needed: len + headroom,
                        free: ring.free(),
                    })
                } else if let Some(gsn) = slot.clock.stamp(stamp_from) {
                    header.gsn = gsn;
                    header.lap = ring.lap();
                    ring.append(header, payload).map(|at| {
                        if slot.in_txn.load(core::sync::atomic::Ordering::SeqCst)
                            && slot.live_from.load(core::sync::atomic::Ordering::SeqCst) == u64::MAX
                        {
                            slot.live_from.store(at, core::sync::atomic::Ordering::SeqCst);
                        }
                        (header.gsn, ring.written())
                    })
                } else {
                    Err(Error::Corrupt)
                }
            };
            let full = match outcome {
                Ok(done) => {
                    self.records.add(1);
                    return Ok(Some(done));
                }
                Err(Error::LogFull { capacity, needed, free }) => (capacity, needed, free),
                Err(other) => return Err(other),
            };
            let (capacity, needed, free) = full;
            last_full = Some(full);
            if needed > capacity {
                return Err(Error::LogFull { capacity, needed, free });
            }
            self.flush(true)?;
            std::thread::yield_now();
        }
        match last_full {
            Some((capacity, needed, free)) => Err(Error::LogFull { capacity, needed, free }),
            None => Err(Error::Livelocked { attempts: APPEND_BOUND }),
        }
    }
    pub fn observed(&self, page_gsn: u64) -> Result<u64, Error> {
        Ok(self.mine()?.clock.observe(page_gsn))
    }
    pub fn written(&self) -> Result<u64, Error> {
        Ok(self.mine()?.ring_ref()?.written())
    }
    pub fn pending(&self) -> Result<usize, Error> {
        Ok(self.mine()?.ring.lock().map_err(|_| Error::Corrupt)?.pending())
    }
}
