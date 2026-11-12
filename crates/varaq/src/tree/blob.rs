use super::insert::Expect;
use super::value::{Value, ValueSink};
use super::{Descent, NODE_WORDS, Resolved, Stop, Tree};
use crate::node::blob::{self, Ref};
use crate::node::reader::NodeRef;
use crate::node::{DATA_BYTES, Node, PageKind};
use crate::{Error, OptimisticGuard, PAGE_SIZE, PageId};
use core::sync::atomic::{AtomicU64, Ordering};
const SERIAL_BLOCK: u64 = 1024;
const SPAN_MOST: usize = 256;
pub(crate) const KEPT_MOST: usize = 16 << 20;
#[derive(Default)]
pub struct BlobRuns {
    serial: AtomicU64,
    stripes: [crate::sync::Padded<std::sync::Mutex<(u64, u64)>>; super::value::PARTITIONS],
    ledger: std::sync::Mutex<super::space::Ledger>,
    runs_staged: crate::stats::Stat,
    pages_read: crate::stats::Stat,
    pages_faulted: crate::stats::Stat,
    runs_reused: crate::stats::Stat,
}
impl core::fmt::Debug for BlobRuns {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlobRuns")
            .field("serial", &self.serial.load(Ordering::Relaxed))
            .field("free", &self.ledger.lock().map(|l| l.free.pages()).ok())
            .field("pending", &self.ledger.lock().map(|l| l.pending.len()).ok())
            .finish_non_exhaustive()
    }
}
pub struct DataBarrier {
    store: std::sync::Arc<dyn crate::store::PageStore>,
    owed: Box<[crate::sync::Padded<core::sync::atomic::AtomicBool>]>,
}
impl core::fmt::Debug for DataBarrier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let owed = self.owed.iter().filter(|o| o.load(Ordering::Relaxed)).count();
        f.debug_struct("DataBarrier").field("owed", &owed).finish_non_exhaustive()
    }
}
impl DataBarrier {
    #[must_use]
    pub fn new(store: std::sync::Arc<dyn crate::store::PageStore>, workers: usize) -> Self {
        let owed = (0..workers.max(1))
            .map(|_| crate::sync::Padded(core::sync::atomic::AtomicBool::new(false)))
            .collect();
        Self { store, owed }
    }
    #[must_use]
    pub fn is_over(&self, store: &std::sync::Arc<dyn crate::store::PageStore>) -> bool {
        std::sync::Arc::ptr_eq(&self.store, store)
    }
    #[must_use]
    pub fn wrote(&self, worker: usize) -> bool {
        self.owed.get(worker).is_some_and(|o| {
            o.store(true, Ordering::Release);
            true
        })
    }
    pub fn cover(&self, worker: usize) -> Result<(), Error> {
        let Some(owed) = self.owed.get(worker) else { return Ok(()) };
        if !owed.load(Ordering::Acquire) || !owed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.store.barrier().inspect_err(|_| owed.store(true, Ordering::Release))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Staged {
    pub(crate) reference: Ref,
    pub(crate) local: usize,
    pub(crate) stamp: u64,
}
impl Staged {
    pub(crate) fn pages(&self) -> u64 {
        blob::pages_for(self.reference.rest(self.local))
    }
}
#[must_use = "a run unnamed and never retired is a leak"]
pub(crate) struct Retiring {
    first: u64,
    pages: u64,
}
pub(crate) enum OnMiss<'k> {
    Resample { key: &'k [u8] },
    Report,
    Owned { span: usize },
}
impl Tree {
    #[must_use]
    pub fn blob_runs_staged(&self) -> u64 {
        self.blobs.runs_staged.get()
    }
    #[must_use]
    pub fn blob_counts(&self) -> (u64, u64, u64) {
        (self.blobs.pages_read.get(), self.blobs.pages_faulted.get(), self.blobs.runs_reused.get())
    }
    #[must_use]
    pub fn blob_serials_issued(&self) -> u64 {
        self.blobs.serial.load(Ordering::Acquire)
    }
    pub fn resume_blob_serials(&self, highest: u64) {
        self.blobs.serial.fetch_max(highest, Ordering::AcqRel);
        for stripe in &self.blobs.stripes {
            *stripe.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (0, 0);
        }
    }
    pub(crate) fn stage_blob(&self, key: &[u8], value: &[u8]) -> Result<Option<Staged>, Error> {
        if !blob::needed(key.len(), value.len()) {
            return Ok(None);
        }
        let local = crate::node::separated::local_len(self.keep_local, value.len());
        if blob::needed(key.len(), blob::REFERENCE_BYTES + local) {
            return Err(Error::EntryTooLarge);
        }
        let len = u64::try_from(value.len()).map_err(|_| Error::EntryTooLarge)?;
        let rest = &value[local..];
        let pages = blob::pages_for(rest.len());
        let (first, stamp) = self.take_run(pages)?;
        let serial = self.next_serial();
        let staged = Staged { reference: Ref { first, len, serial }, local, stamp };
        if let Err(e) = self.write_run(staged, rest) {
            self.unstage(staged);
            return Err(e);
        }
        self.blobs.runs_staged.add(1);
        Ok(Some(staged))
    }
    fn next_serial(&self) -> u64 {
        let stripe = &self.blobs.stripes[super::value::Room::partition()];
        let mut block = stripe.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if block.0 >= block.1 {
            let end = self.blobs.serial.fetch_add(SERIAL_BLOCK, Ordering::AcqRel) + SERIAL_BLOCK;
            *block = (end - SERIAL_BLOCK + 1, end + 1);
        }
        let serial = block.0;
        block.0 += 1;
        serial
    }
    fn write_run(&self, staged: Staged, rest: &[u8]) -> Result<(), Error> {
        thread_local! {
            static STAGING : core::cell::Cell < Vec < u8 >> = const {
            core::cell::Cell::new(Vec::new()) };
        }
        let pages = usize::try_from(staged.pages()).map_err(|_| Error::EntryTooLarge)?;
        let bytes = pages.checked_mul(PAGE_SIZE).ok_or(Error::EntryTooLarge)?;
        let mut buf = STAGING.with(core::cell::Cell::take);
        buf.resize(bytes, 0);
        let Ref { first, serial, .. } = staged.reference;
        for (index, (out, chunk)) in buf
            .as_chunks_mut::<PAGE_SIZE>()
            .0
            .iter_mut()
            .zip(rest.chunks(DATA_BYTES).chain(core::iter::repeat(&[][..])))
            .enumerate()
        {
            let index = index as u64;
            blob::write_page(out, serial, index, chunk);
            crate::page::checksum::stamp(out, PageId::new(first + index)?);
        }
        let written = self.store.write_run(PageId::new(first)?, &buf);
        if buf.capacity() <= KEPT_MOST {
            STAGING.with(|cell| cell.set(buf));
        }
        written?;
        if self.wal.is_some() {
            let worker = crate::wal::worker::current().map(usize::from);
            let owed = self.data_barrier.as_ref().zip(worker).is_some_and(|(b, w)| b.wrote(w));
            if !owed {
                self.store.barrier()?;
            }
        }
        Ok(())
    }
    fn take_run(&self, pages: u64) -> Result<(u64, u64), Error> {
        let mut ledger = self.blobs.ledger.lock().map_err(|_| Error::Poisoned)?;
        self.release_durable(&mut ledger);
        let taken = match ledger.free.take(pages) {
            Some(found) => Some(found),
            None => self.join_at_end(&mut ledger.free, pages)?,
        };
        let (first, stamp) = match taken {
            Some(found) => found,
            None => (self.store.allocate_run(pages)?.get(), 0),
        };
        if self.wal.is_some() {
            ledger.in_flight.insert(first, (pages, stamp, None));
        }
        drop(ledger);
        if taken.is_some() {
            self.blobs.runs_reused.add(1);
        }
        if let Some(wal) = self.wal.as_ref() {
            let _ = wal.observed(stamp);
        }
        Ok((first, stamp))
    }
    fn here(&self, gsn: u64) -> Option<super::space::At> {
        let wal = self.wal.as_ref()?;
        let worker = crate::wal::worker::current().map(usize::from)?;
        Some(super::space::At { worker, end: wal.written().ok()?, gsn })
    }
    pub(crate) fn published(&self, staged: Staged, gsn: Option<u64>) {
        let Some(at) = gsn.and_then(|g| self.here(g)) else { return };
        if let Ok(mut ledger) = self.blobs.ledger.lock()
            && let Some(entry) = ledger.in_flight.get_mut(&staged.reference.first)
        {
            entry.2 = Some(at);
        }
    }
    fn join_at_end(
        &self,
        free: &mut super::space::Space,
        pages: u64,
    ) -> Result<Option<(u64, u64)>, Error> {
        let last = self.store.len() as u64;
        let Some((first, held, stamp)) = free.take_ending_at(last) else { return Ok(None) };
        let more = match self.store.allocate_run(pages - held) {
            Ok(more) => more.get(),
            Err(e) => {
                free.give(first, held, stamp);
                return Err(e);
            }
        };
        if more == last + 1 {
            return Ok(Some((first, stamp)));
        }
        free.give(first, held, stamp);
        free.give(more, pages - held, 0);
        Ok(None)
    }
    fn release_durable(&self, ledger: &mut super::space::Ledger) {
        let Some(wal) = self.wal.as_ref() else { return };
        if ledger.pending.is_empty() && ledger.in_flight.is_empty() {
            return;
        }
        let flushed = wal.flushed();
        ledger.release(|gsn| flushed.may_write_back(gsn));
    }
    pub(crate) fn unstage(&self, staged: Staged) {
        if let Ok(mut ledger) = self.blobs.ledger.lock() {
            ledger.in_flight.remove(&staged.reference.first);
            ledger.free.give(staged.reference.first, staged.pages(), staged.stamp);
        }
    }
    pub(crate) fn retiring(
        &self,
        reference: Ref,
        local: usize,
        (worker, gsn): (usize, u64),
    ) -> Retiring {
        let cause = self.here(gsn).unwrap_or(super::space::At { worker, end: u64::MAX, gsn });
        let pages = blob::pages_for(reference.rest(local));
        if self.wal.is_some()
            && let Ok(mut ledger) = self.blobs.ledger.lock()
        {
            let first = reference.first;
            let release = gsn;
            ledger.pending.push(super::space::Pending {
                first,
                pages,
                release,
                cause,
                forgot: false,
            });
        }
        Retiring { first: reference.first, pages }
    }
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn retire_unnamed(&self, retiring: Retiring) {
        let Retiring { first, pages } = retiring;
        let forgotten = self.forget_run(first, pages);
        let Ok(mut ledger) = self.blobs.ledger.lock() else { return };
        if self.wal.is_none() {
            if forgotten {
                ledger.free.give(first, pages, 0);
            }
            return;
        }
        if let Some(at) = ledger.pending.iter().rposition(|p| p.first == first && !p.forgot) {
            if forgotten {
                ledger.pending[at].forgot = true;
            } else {
                ledger.pending.swap_remove(at);
            }
        }
    }
    fn forget_run(&self, first: u64, pages: u64) -> bool {
        (0..pages).all(|i| {
            let Ok(page) = PageId::new(first.saturating_add(i)) else { return false };
            (0..super::RETRY_BOUND).any(|_| {
                let Ok(found) = self.predict(page) else {
                    crate::latch::spin();
                    return false;
                };
                let Some((latch, frame, sample)) = found else { return true };
                let Ok(held) = latch.try_exclusive() else {
                    crate::latch::spin();
                    return false;
                };
                let resolved = Resolved { latch, page, frame, sample };
                if self.confirm(&resolved).is_err()
                    || !self.holds_named(frame, page)
                    || !self.no_writeback_in_flight(frame)
                {
                    drop(held);
                    crate::latch::spin();
                    return false;
                }
                let Some(slot) = self.pool.slot(frame) else { return false };
                let reclaimed = slot.frame.reclaim().is_ok();
                crate::bufmgr::Children::forget(self, page, frame);
                if reclaimed {
                    let _ = self.pool.release(frame);
                }
                drop(held);
                reclaimed
            })
        })
    }
    pub(crate) fn blob_into(
        &self,
        reference: Ref,
        local: &[u8],
        miss: &OnMiss<'_>,
        out: &mut (impl ValueSink + ?Sized),
    ) -> Descent<Value> {
        let rest = reference.rest(local.len());
        let room = Joined { local, out }.sized(rest);
        if room.is_empty() {
            return Ok(Value::Read(local.len() + rest));
        }
        let walked = self.walk_run(reference, rest, miss, |node, at, held| {
            let end = (at + held).min(room.len());
            if at < end {
                node.blob_into(&mut room[at..end])?;
            }
            Ok(end < room.len())
        })?;
        Ok(match walked {
            Some(page) => Value::Absent(page),
            None => Value::Read(local.len() + rest),
        })
    }
    fn walk_run(
        &self,
        reference: Ref,
        rest: usize,
        miss: &OnMiss<'_>,
        mut visit: impl FnMut(&NodeRef<'_, '_, NODE_WORDS>, usize, usize) -> crate::Optimistic<bool>,
    ) -> Descent<Option<PageId>> {
        let pages = blob::pages_for(rest);
        let mut at = 0_usize;
        let mut index = 0_u64;
        let mut missed = 0_u32;
        let again = |missed: &mut u32| {
            *missed += 1;
            if *missed >= super::RETRY_BOUND {
                return Err(Stop::Refuse(Error::Livelocked { attempts: super::RETRY_BOUND }));
            }
            if *missed > 1 {
                self.yield_or_spin(*missed);
            }
            Ok(())
        };
        while index < pages {
            let page = PageId::new(reference.first.saturating_add(index))
                .map_err(|_| Stop::Refuse(Error::Corrupt))?;
            let Ok(found) = self.predict(page) else {
                again(&mut missed)?;
                continue;
            };
            let Some((latch, frame, sample)) = found else {
                match miss {
                    OnMiss::Report => return Ok(Some(page)),
                    OnMiss::Owned { span } => {
                        let span = usize::try_from(pages - index).unwrap_or(SPAN_MOST).min(*span);
                        let held = crate::bufmgr::fault::Route::Held;
                        if self.read_span(page, span, held).map_err(Stop::Refuse)? == 0 {
                            self.blobs.pages_faulted.add(1);
                            self.with_room(|| self.read_page(page, held))
                                .map_err(super::unnamed_is_a_retry)?;
                        }
                        continue;
                    }
                    OnMiss::Resample { key } => {
                        again(&mut missed)?;
                        let span = usize::try_from(pages - index).unwrap_or(usize::MAX);
                        self.fault_resampled(key, reference, page, span)?;
                        continue;
                    }
                }
            };
            let resolved = Resolved { latch, page, frame, sample };
            let Ok(guard) = resolved.latch.optimistic() else {
                Self::refused_by(resolved.latch);
                again(&mut missed)?;
                continue;
            };
            if self.confirm(&resolved).is_err() {
                again(&mut missed)?;
                continue;
            }
            let node = NodeRef::new(&guard);
            let want = (rest - at).min(DATA_BYTES);
            let checked = node.blob_header().and_then(|(serial, number, held)| {
                Ok((
                    node.kind()? == PageKind::Blob as u8
                        && serial == reference.serial
                        && number == index
                        && held == want,
                    held,
                ))
            });
            let more = match checked {
                Ok((true, held)) => visit(&node, at, held).map(|more| (more, held)),
                Ok((false, _)) if guard.recheck().is_ok() => {
                    if let OnMiss::Resample { key } = miss {
                        let super::Leaf { guard: leaf, .. } = self.descend(key)?;
                        if !Self::names_run(key, reference, &leaf)? {
                            return Err(Stop::Retry);
                        }
                    }
                    return Err(Stop::Refuse(Error::Corrupt));
                }
                Ok((false, _)) | Err(_) => Err(crate::Unwind),
            };
            let Ok((more, held)) = more.and_then(|m| guard.recheck().map(|()| m)) else {
                again(&mut missed)?;
                continue;
            };
            missed = 0;
            self.blobs.pages_read.add(1);
            if !more {
                break;
            }
            at += held;
            index += 1;
        }
        Ok(None)
    }
    fn names_run(
        key: &[u8],
        reference: Ref,
        leaf: &OptimisticGuard<'_, Node, NODE_WORDS>,
    ) -> Descent<bool> {
        let node = NodeRef::new(leaf);
        let (at, hit) = node.lower_bound(key)?;
        let named = hit
            && node.slot(at)?.4 & crate::node::flags::BLOB != 0
            && super::value::Blob::read(&node, at)?.is_some_and(|(r, _)| r == reference);
        leaf.recheck()?;
        Ok(named)
    }
    fn fault_resampled(
        &self,
        key: &[u8],
        reference: Ref,
        page: PageId,
        span: usize,
    ) -> Descent<()> {
        let super::Leaf { frame, guard, .. } = self.descend(key)?;
        if !Self::names_run(key, reference, &guard)? {
            return Err(Stop::Retry);
        }
        match self.fault_run(page, span, frame, &guard) {
            Err(Stop::Retry) => Ok(()),
            other => other,
        }
    }
    pub(crate) fn blob_read(
        &self,
        key: &[u8],
        reference: Ref,
        local: &[u8],
        out: &mut (impl ValueSink + ?Sized),
    ) -> Descent<usize> {
        match self.blob_into(reference, local, &OnMiss::Resample { key }, out)? {
            Value::Read(len) => Ok(len),
            Value::Absent(_) => Err(Stop::Retry),
        }
    }
    pub(crate) fn fault_run(
        &self,
        page: PageId,
        span: usize,
        leaf_frame: u32,
        leaf: &OptimisticGuard<'_, Node, NODE_WORDS>,
    ) -> Descent<()> {
        leaf.recheck()?;
        let route =
            crate::bufmgr::fault::Route::Parent { frame: leaf_frame, version: leaf.sampled() };
        match self.read_span(page, span.min(SPAN_MOST), route) {
            Ok(loaded) if loaded > 0 => {
                self.blobs.pages_faulted.add(loaded as u64);
                Ok(())
            }
            Ok(_) => {
                self.blobs.pages_faulted.add(1);
                self.fault_value(page, leaf_frame, leaf)
            }
            Err(e) => {
                leaf.recheck()?;
                Err(Stop::Refuse(e))
            }
        }
    }
    pub(super) fn blob_matches(
        &self,
        expect: Expect<'_>,
        reference: Ref,
        local: &[u8],
        miss: &OnMiss<'_>,
    ) -> Descent<Result<bool, PageId>> {
        let len = usize::try_from(reference.len).unwrap_or(usize::MAX);
        let want = match expect {
            Expect::Anything => return Ok(Ok(true)),
            Expect::Blob(r, kept) => return Ok(Ok(r == reference && kept == local)),
            Expect::Exactly(None) => return Ok(Ok(false)),
            Expect::Exactly(Some(want)) if want.len() != len => return Ok(Ok(false)),
            Expect::Exactly(Some(want)) => want,
            Expect::Head(head) if head.len() > len => return Ok(Ok(false)),
            Expect::Head(head) => head,
        };
        let here = want.len().min(local.len());
        if want[..here] != local[..here] {
            return Ok(Ok(false));
        }
        let want = &want[here..];
        if want.is_empty() {
            return Ok(Ok(true));
        }
        let mut same = true;
        let mut page = [0_u8; DATA_BYTES];
        let walked =
            self.walk_run(reference, reference.rest(local.len()), miss, |node, at, held| {
                let n = held.min(want.len() - at);
                node.blob_into(&mut page[..n])?;
                same &= page[..n] == want[at..at + n];
                Ok(same && at + n < want.len())
            })?;
        Ok(match walked {
            Some(page) => Err(page),
            None => Ok(same),
        })
    }
}
struct Joined<'a, S: ValueSink + ?Sized> {
    local: &'a [u8],
    out: &'a mut S,
}
impl<'a, S: ValueSink + ?Sized> Joined<'a, S> {
    fn sized(self, rest: usize) -> &'a mut [u8] {
        let n = self.local.len();
        let room = self.out.sized(n + rest);
        let kept = n.min(room.len());
        room[..kept].copy_from_slice(&self.local[..kept]);
        &mut room[kept..]
    }
}
impl Tree {
    pub(crate) fn compare_stored_and_insert_logged(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        value: &[u8],
        undo: super::TxnUndo<'_>,
    ) -> Result<bool, Error> {
        match expected.and_then(blob::of_token) {
            Some((reference, kept)) => self.modifying(|| {
                self.insert_expecting(key, value, Expect::Blob(reference, kept), Some(undo))
            }),
            None => self.compare_and_insert_logged(key, expected, value, undo),
        }
    }
    pub fn restore_stored(&self, key: &[u8], head: &[u8], before: &[u8]) -> Result<bool, Error> {
        let Some((reference, kept)) = blob::of_token(before) else {
            return self.compare_head_and_insert(key, head, before);
        };
        let staged = Staged { reference, local: kept.len(), stamp: 0 };
        self.modifying(|| self.insert_staged(key, kept, Expect::Head(head), None, Some(staged)))
    }
    pub(crate) fn value_of_stored(&self, stored: &[u8]) -> Result<Vec<u8>, Error> {
        let Some((reference, kept)) = blob::of_token(stored) else {
            return Ok(stored.to_vec());
        };
        let mut out = Vec::new();
        let miss = OnMiss::Owned { span: SPAN_MOST };
        self.retrying(|| match self.blob_into(reference, kept, &miss, &mut out)? {
            Value::Read(_) => Ok(()),
            Value::Absent(_) => Err(Stop::Retry),
        })?;
        Ok(out)
    }
    pub(crate) fn stored_prefix(&self, stored: &[u8], into: &mut [u8]) -> Result<usize, Error> {
        let Some((reference, kept)) = blob::of_token(stored) else {
            let n = into.len().min(stored.len());
            into[..n].copy_from_slice(&stored[..n]);
            return Ok(n);
        };
        let want = into.len().min(usize::try_from(reference.len).unwrap_or(usize::MAX));
        let here = want.min(kept.len());
        into[..here].copy_from_slice(&kept[..here]);
        let room = &mut into[here..want];
        if !room.is_empty() {
            let rest = reference.rest(kept.len());
            let miss = OnMiss::Owned { span: 1 };
            self.retrying(|| {
                self.walk_run(reference, rest, &miss, |node, at, held| {
                    let end = (at + held).min(room.len());
                    node.blob_into(&mut room[at..end])?;
                    Ok(end < room.len())
                })
                .map(|_| ())
            })?;
        }
        Ok(want)
    }
    pub(crate) fn retire_stored(&self, stored: &[u8], worker: usize) {
        let Some((reference, kept)) = blob::of_token(stored) else { return };
        let first = reference.first;
        let pages = blob::pages_for(reference.rest(kept.len()));
        let behind = self
            .wal
            .as_ref()
            .and_then(|w| w.slots_ref().get(worker).map(|s| s.clock.now()))
            .unwrap_or(u64::MAX);
        if let Ok(mut ledger) = self.blobs.ledger.lock() {
            let transfer = ledger.owned.remove(&first).map(|(_, at)| at);
            if self.wal.is_some() {
                let cause =
                    transfer.unwrap_or(super::space::At { worker, end: u64::MAX, gsn: behind });
                ledger.pending.push(super::space::Pending {
                    first,
                    pages,
                    release: behind,
                    cause,
                    forgot: false,
                });
            }
        }
        self.retire_unnamed(Retiring { first, pages });
    }
    pub fn transferred(&self, stored: &[u8], gsn: u64) {
        let Some((reference, kept)) = blob::of_token(stored) else { return };
        let Some(at) = self.here(gsn) else { return };
        let pages = blob::pages_for(reference.rest(kept.len()));
        if let Ok(mut ledger) = self.blobs.ledger.lock() {
            ledger.owned.insert(reference.first, (pages, at));
        }
    }
    #[must_use]
    pub fn blob_space_listing(&self, begins: &[u64]) -> Vec<(u64, u64, u64)> {
        let Some(wal) = self.wal.as_ref() else { return Vec::new() };
        let flushed = wal.flushed();
        self.blobs
            .ledger
            .lock()
            .map(|mut l| l.listing(begins, |g| flushed.may_write_back(g)))
            .unwrap_or_default()
    }
    pub fn seed_blob_space(&self, free: &[(u64, u64)], stamp: u64) {
        if let Ok(mut ledger) = self.blobs.ledger.lock() {
            for &(first, pages) in free {
                ledger.free.give(first, pages, stamp);
            }
        }
    }
    #[must_use]
    pub fn blob_pages_free(&self) -> u64 {
        self.blobs.ledger.lock().map_or(0, |l| l.free.pages())
    }
    #[must_use]
    pub fn blob_pages_pending(&self) -> u64 {
        self.blobs.ledger.lock().map_or(0, |l| l.pending.iter().map(|p| p.pages).sum())
    }
    #[must_use]
    pub fn blob_pages_owned(&self) -> u64 {
        self.blobs.ledger.lock().map_or(0, |l| l.owned.values().map(|&(pages, _)| pages).sum())
    }
    pub(super) fn stamped(stamp: Option<u64>) -> (usize, u64) {
        let worker = crate::wal::worker::current().map_or(0, usize::from);
        (worker, stamp.unwrap_or(u64::MAX))
    }
}
impl Tree {
    pub(super) fn blob_expectation(
        &self,
        key: &[u8],
        expect: Expect<'_>,
        leaf_frame: u32,
        guard: &OptimisticGuard<'_, Node, NODE_WORDS>,
    ) -> Descent<((usize, bool), Option<bool>)> {
        let node = NodeRef::new(guard);
        let (at, hit) = node.lower_bound(key)?;
        if !hit || node.slot(at)?.4 & crate::node::flags::BLOB == 0 {
            return Ok(((at, hit), None));
        }
        if matches!(expect, Expect::Anything) {
            return Ok(((at, hit), Some(true)));
        }
        let Some((reference, kept)) = super::value::Blob::read(&node, at)? else {
            return Err(Stop::Retry);
        };
        guard.recheck()?;
        let _ = leaf_frame;
        let miss = OnMiss::Resample { key };
        match self.blob_matches(expect, reference, kept.bytes(), &miss)? {
            Ok(same) => Ok(((at, hit), Some(same))),
            Err(_) => Err(Stop::Retry),
        }
    }
}
