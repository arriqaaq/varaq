use super::insert::{Attempt, Expect};
use super::{Descent, Fresh, Leaf, NODE_WORDS, Stop, Tree};
use crate::bufmgr::fault::Route;
use crate::bufmgr::pool::PageGuard;
use crate::latch::ExclusiveGuard;
use crate::node::reader::NodeRef;
use crate::node::{Node, PageKind};
use crate::{Error, OptimisticGuard, PageId};
fn of_kind(page: &Node) -> Result<(), Error> {
    if page.kind() == PageKind::Value as u8 { Ok(()) } else { Err(Error::Corrupt) }
}
pub fn place(page: &mut Node, head: &[u8], tail: &[u8], value: &[u8]) -> Result<bool, Error> {
    place_reporting(page, head, tail, value).map(|(placed, _)| placed)
}
pub fn place_reporting(
    page: &mut Node,
    head: &[u8],
    tail: &[u8],
    value: &[u8],
) -> Result<(bool, bool), Error> {
    of_kind(page)?;
    Ok(page.insert_parts_reporting(head, tail, value, 0))
}
pub fn read<'p>(page: &'p Node, owner: &[u8]) -> Result<Option<&'p [u8]>, Error> {
    of_kind(page)?;
    Ok(match page.lower_bound(owner) {
        (at, true) => Some(page.payload_at(at)),
        (_, false) => None,
    })
}
pub fn free(page: &mut Node, owner: &[u8]) -> Result<bool, Error> {
    of_kind(page)?;
    Ok(match page.lower_bound(owner) {
        (at, true) => page.remove_slot(at),
        (_, false) => false,
    })
}
impl Tree {
    pub fn retire_value_page(
        &self,
        frame: u32,
        page: PageId,
        held: &ExclusiveGuard<'_, Node, NODE_WORDS>,
    ) -> bool {
        let node = held.get();
        if node.kind() != PageKind::Value as u8 || !node.is_empty() {
            return false;
        }
        if !self.named_here(frame, page) || !self.no_writeback_in_flight(frame) {
            return false;
        }
        self.room.forget(page);
        self.retire(frame, Some(PageKind::Value as u8));
        true
    }
    pub(crate) fn separated_into(
        &self,
        reference: u64,
        local: &[u8],
        owner: &[u8],
        leaf: &OptimisticGuard<'_, Node, NODE_WORDS>,
        out: &mut (impl ValueSink + ?Sized),
    ) -> Descent<Value> {
        let Ok(page) = PageId::new(reference) else {
            leaf.recheck()?;
            return Err(Stop::Refuse(Error::Corrupt));
        };
        let Some((latch, frame, sample)) = self.predict(page)? else {
            leaf.recheck()?;
            return Ok(Value::Absent(page));
        };
        let resolved = super::Resolved { latch, page, frame, sample };
        let value =
            resolved.latch.optimistic().inspect_err(|_| Self::refused_by(resolved.latch))?;
        self.confirm(&resolved)?;
        let node = NodeRef::new(&value);
        let at = if node.kind()? == PageKind::Value as u8 {
            match node.lower_bound(owner)? {
                (at, true) => Some(at),
                (_, false) => None,
            }
        } else {
            None
        };
        let Some(at) = at else {
            value.recheck()?;
            leaf.recheck()?;
            return Err(Stop::Refuse(Error::Corrupt));
        };
        let len = copy_entry(&node, at, &mut Joined { local, out })?;
        value.recheck()?;
        self.value_reads.add(1);
        Ok(Value::Read(local.len() + len))
    }
    pub(crate) fn fault_value(
        &self,
        page: PageId,
        leaf_frame: u32,
        leaf: &OptimisticGuard<'_, Node, NODE_WORDS>,
    ) -> Descent<()> {
        let route = Route::Parent { frame: leaf_frame, version: leaf.sampled() };
        match self.fault_value_page(page, route) {
            Err(Stop::Refuse(e)) => {
                leaf.recheck()?;
                Err(Stop::Refuse(e))
            }
            other => other,
        }
    }
    fn fault_value_page(&self, page: PageId, route: Route) -> Descent<()> {
        self.fault_predictive(page, route).inspect_err(|stop| {
            if matches!(stop, Stop::Retry) {
                self.value_copies_unnamed.add(1);
            }
        })
    }
    pub(crate) fn entry_value_into(
        &self,
        leaf: &NodeRef<'_, '_, NODE_WORDS>,
        guard: &OptimisticGuard<'_, Node, NODE_WORDS>,
        leaf_frame: u32,
        (at, flags): (usize, u16),
        owner: &[u8],
        out: &mut (impl ValueSink + ?Sized),
    ) -> Descent<usize> {
        if flags & crate::node::flags::OUT_OF_LINE == 0 {
            return Ok(copy_entry(leaf, at, out)?);
        }
        if flags & crate::node::flags::BLOB != 0 {
            debug_assert!(false, "a blob reached the inline and separated read");
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let Some(entry) = Separated::read(leaf, at)? else {
            self.retried(RetrySite::TornReference);
            return Err(Stop::Retry);
        };
        match self.separated_into(entry.page, entry.local(), owner, guard, out)? {
            Value::Read(len) => Ok(len),
            Value::Absent(page) => {
                self.retried(RetrySite::ReadFault);
                self.fault_value(page, leaf_frame, guard)?;
                Err(Stop::Retry)
            }
        }
    }
}
impl Tree {
    pub(crate) fn entry_value_held<'t>(
        &'t self,
        leaf: &NodeRef<'_, '_, NODE_WORDS>,
        at: usize,
        owner: &[u8],
        out: &mut (impl ValueSink + ?Sized),
        announced: &mut super::Announced<'t>,
    ) -> Descent<Value> {
        let (_, _, _, _, flags) = leaf.slot(at)?;
        if flags & crate::node::flags::OUT_OF_LINE == 0 {
            return Ok(Value::Read(copy_entry(leaf, at, out)?));
        }
        if flags & crate::node::flags::BLOB != 0 {
            debug_assert!(false, "a blob reached the held separated read");
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let Some(entry) = Separated::read(leaf, at)? else {
            return Err(Stop::Refuse(Error::Corrupt));
        };
        self.separated_held(entry.page, entry.local(), owner, out, announced)
    }
    pub(crate) fn separated_held<'t>(
        &'t self,
        reference: u64,
        local: &[u8],
        owner: &[u8],
        out: &mut (impl ValueSink + ?Sized),
        announced: &mut super::Announced<'t>,
    ) -> Descent<Value> {
        let page = PageId::new(reference).map_err(|_| Stop::Refuse(Error::Corrupt))?;
        let Some((latch, frame, sample)) = self.predict(page)? else {
            return Ok(Value::Absent(page));
        };
        let resolved = super::Resolved { latch, page, frame, sample };
        let value = resolved.latch.optimistic().inspect_err(|_| {
            announced.on(resolved.latch);
            Self::refused_by(resolved.latch);
        })?;
        self.confirm(&resolved)?;
        let value = value.try_shared().inspect_err(|_| {
            announced.on(resolved.latch);
            Self::refused_by(resolved.latch);
        })?;
        let node = NodeRef::shared(&value);
        if node.kind()? != PageKind::Value as u8 {
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let (at, found) = node.lower_bound(owner)?;
        if !found {
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let len = copy_entry(&node, at, &mut Joined { local, out })?;
        self.value_reads.add(1);
        Ok(Value::Read(local.len() + len))
    }
}
pub(crate) struct Separated {
    pub(crate) page: u64,
    local: [u8; crate::node::separated::LOCAL_MOST],
    len: usize,
}
impl Separated {
    pub(crate) fn read<const W: usize>(
        leaf: &NodeRef<'_, '_, W>,
        at: usize,
    ) -> crate::Optimistic<Option<Self>> {
        use crate::node::separated::{PAYLOAD_MOST, well_formed_len};
        let (_, _, payload_len, _, _) = leaf.slot(at)?;
        if !well_formed_len(payload_len) {
            return Ok(None);
        }
        let mut raw = [0_u8; PAYLOAD_MOST];
        leaf.payload_at(at, &mut raw[..payload_len])?;
        Ok(Self::of(&raw[..payload_len]))
    }
    pub(crate) fn of(payload: &[u8]) -> Option<Self> {
        let (page, kept) = crate::node::separated::decode(payload)?;
        let mut local = [0_u8; crate::node::separated::LOCAL_MOST];
        local[..kept.len()].copy_from_slice(kept);
        Some(Self { page, local, len: kept.len() })
    }
    pub(crate) fn local(&self) -> &[u8] {
        &self.local[..self.len]
    }
}
pub(crate) struct Blob;
pub(crate) struct Kept {
    bytes: [u8; crate::node::blob::LOCAL_MOST],
    len: usize,
}
impl Kept {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
impl Blob {
    pub(crate) fn read<const W: usize>(
        leaf: &NodeRef<'_, '_, W>,
        at: usize,
    ) -> crate::Optimistic<Option<(crate::node::blob::Ref, Kept)>> {
        use crate::node::blob::{PAYLOAD_MOST, well_formed_len};
        let (_, _, payload_len, _, _) = leaf.slot(at)?;
        if !well_formed_len(payload_len) {
            return Ok(None);
        }
        let mut raw = [0_u8; PAYLOAD_MOST];
        leaf.payload_at(at, &mut raw[..payload_len])?;
        Ok(Self::of(&raw[..payload_len]))
    }
    pub(crate) fn of(payload: &[u8]) -> Option<(crate::node::blob::Ref, Kept)> {
        let (reference, kept) = crate::node::blob::decode(payload)?;
        let mut bytes = [0_u8; crate::node::blob::LOCAL_MOST];
        bytes[..kept.len()].copy_from_slice(kept);
        Some((reference, Kept { bytes, len: kept.len() }))
    }
}
struct Joined<'a, S: ValueSink + ?Sized> {
    local: &'a [u8],
    out: &'a mut S,
}
impl<S: ValueSink + ?Sized> ValueSink for Joined<'_, S> {
    fn sized(&mut self, len: usize) -> &mut [u8] {
        let n = self.local.len();
        let room = self.out.sized(n + len);
        let kept = n.min(room.len());
        room[..kept].copy_from_slice(&self.local[..kept]);
        &mut room[kept..]
    }
    fn holds(&self, len: usize) -> bool {
        self.out.holds(self.local.len() + len)
    }
}
pub(crate) trait ValueSink {
    fn sized(&mut self, len: usize) -> &mut [u8];
    fn holds(&self, len: usize) -> bool {
        let _ = len;
        true
    }
}
impl ValueSink for [u8] {
    fn sized(&mut self, len: usize) -> &mut [u8] {
        let n = len.min(self.len());
        &mut self[..n]
    }
    fn holds(&self, len: usize) -> bool {
        len <= self.len()
    }
}
pub(crate) struct Append<'b> {
    buf: &'b mut Vec<u8>,
    at: usize,
}
impl<'b> Append<'b> {
    pub(crate) fn after(buf: &'b mut Vec<u8>) -> Self {
        let at = buf.len();
        Self { buf, at }
    }
}
impl ValueSink for Append<'_> {
    fn sized(&mut self, len: usize) -> &mut [u8] {
        self.buf.resize(self.at + len, 0);
        &mut self.buf[self.at..]
    }
}
impl ValueSink for Vec<u8> {
    fn sized(&mut self, len: usize) -> &mut [u8] {
        self.resize(len, 0);
        self
    }
}
pub(crate) struct Probe<'b> {
    small: [u8; 128],
    big: &'b mut Vec<u8>,
}
impl<'b> Probe<'b> {
    pub(crate) fn lending(big: &'b mut Vec<u8>) -> Self {
        Self { small: [0; 128], big }
    }
    pub(crate) fn bytes(&self, len: usize) -> &[u8] {
        if len <= self.small.len() {
            &self.small[..len]
        } else {
            &self.big[..len.min(self.big.len())]
        }
    }
}
impl ValueSink for Probe<'_> {
    fn sized(&mut self, len: usize) -> &mut [u8] {
        if len <= self.small.len() { &mut self.small[..len] } else { self.big.sized(len) }
    }
    fn holds(&self, len: usize) -> bool {
        self.bytes(len).len() == len
    }
}
fn copy_entry(
    node: &NodeRef<'_, '_, NODE_WORDS>,
    at: usize,
    out: &mut (impl ValueSink + ?Sized),
) -> crate::Optimistic<usize> {
    let (_, _, len, _, _) = node.slot(at)?;
    node.payload_at(at, out.sized(len))
}
pub(crate) enum Value {
    Read(usize),
    Absent(PageId),
}
#[derive(Clone, Copy, Debug)]
pub enum RetrySite {
    PageBusy = 0,
    Refault = 1,
    EmptyingWritten = 2,
    NoLongerSeparated = 3,
    HintUnusable = 4,
    ReadFault = 5,
    TornReference = 6,
}
pub const RETRY_SITES: usize = 7;
pub(crate) const PARTITIONS: usize = 8;
const ROOM_BOUND: usize = 64;
#[derive(Default)]
pub struct Room {
    open: [Open; PARTITIONS],
    room: std::sync::Mutex<Vec<PageId>>,
}
#[derive(Default)]
#[repr(align(128))]
struct Open(std::sync::Mutex<Option<PageId>>);
const _: () = assert!(align_of::<Open>() == 128 && size_of::<Open>() == 128);
impl core::ops::Deref for Open {
    type Target = std::sync::Mutex<Option<PageId>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Room {
    pub(crate) fn partition() -> usize {
        static NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        thread_local! {
            static PARTITION : usize = NEXT.fetch_add(1,
            core::sync::atomic::Ordering::Relaxed);
        }
        PARTITION.with(|p| *p % PARTITIONS)
    }
    fn candidate(&self) -> Option<PageId> {
        let open = self.open[Self::partition()].lock().ok().and_then(|o| *o);
        open.or_else(|| self.room.lock().ok().and_then(|r| r.last().copied()))
    }
    pub(crate) fn opened(&self, page: PageId) {
        if let Ok(mut open) = self.open[Self::partition()].lock() {
            *open = Some(page);
        }
    }
    pub(crate) fn admit(&self, page: PageId) {
        if let Ok(mut room) = self.room.lock()
            && !room.contains(&page)
            && room.len() < ROOM_BOUND
        {
            room.push(page);
        }
    }
    pub(crate) fn forget(&self, page: PageId) {
        for open in &self.open {
            if let Ok(mut open) = open.lock()
                && *open == Some(page)
            {
                *open = None;
            }
        }
        if let Ok(mut room) = self.room.lock() {
            room.retain(|&p| p != page);
        }
    }
    #[must_use]
    pub fn names(&self, page: PageId) -> bool {
        self.open.iter().any(|o| o.lock().is_ok_and(|o| *o == Some(page)))
            || self.room.lock().is_ok_and(|r| r.contains(&page))
    }
}
struct Held<'t> {
    frame: u32,
    page: PageId,
    guard: PageGuard<'t>,
}
impl Tree {
    pub(super) fn retried(&self, site: RetrySite) {
        self.value_retries[site as usize].add(1);
    }
    fn value_latched(&self, page: PageId) -> Descent<Option<Held<'_>>> {
        let Some((latch, frame, sample)) =
            self.predict(page).inspect_err(|_| self.retried(RetrySite::PageBusy))?
        else {
            return Ok(None);
        };
        let guard = latch.try_exclusive().inspect_err(|_| {
            self.retried(RetrySite::PageBusy);
            Self::refused_by(latch);
        })?;
        let resolved = super::Resolved { latch, page, frame, sample };
        if self.confirm(&resolved).is_err() || !self.holds_named(frame, page) {
            self.retried(RetrySite::PageBusy);
            return Err(Stop::Retry);
        }
        Ok(Some(Held { frame, page, guard }))
    }
    pub(super) fn try_insert_value(
        &self,
        key: &[u8],
        value: &[u8],
        expect: Expect<'_>,
        undo: Option<super::TxnUndo<'_>>,
        staged: Option<super::blob::Staged>,
    ) -> Descent<Attempt> {
        let separate =
            staged.is_none() && crate::node::separates(self.separate_above, key.len(), value.len());
        let mut fresh = None;
        let mut outcome =
            self.write_value(key, value, (expect, undo), (separate, staged), true, &mut fresh);
        if matches!(outcome, Ok(Attempt::NeedsPage)) {
            let mut page = Node::new(PageKind::Value, b"", b"");
            let rest = &value[crate::node::separated::local_len(self.keep_local, value.len())..];
            if place(&mut page, key, b"", rest) != Ok(true) {
                return Err(Stop::Refuse(Error::EntryTooLarge));
            }
            fresh = Some(self.allocate(&page)?);
            outcome =
                self.write_value(key, value, (expect, undo), (separate, staged), false, &mut fresh);
        }
        if let Some(unused) = fresh.take() {
            self.retire(unused.frame(), None);
            drop(unused);
        }
        match outcome {
            Ok(Attempt::NeedsPage) => Err(Stop::Retry),
            other => other,
        }
    }
    #[allow(clippy::too_many_lines)]
    fn write_value<'t>(
        &'t self,
        key: &[u8],
        value: &[u8],
        (expect, undo): (Expect<'_>, Option<super::TxnUndo<'_>>),
        (separate, staged): (bool, Option<super::blob::Staged>),
        hinted: bool,
        fresh: &mut Option<Fresh<'t>>,
    ) -> Descent<Attempt> {
        use crate::node::flags::{BLOB, SEPARATED};
        let Leaf { frame: leaf_frame, latch: leaf_latch, guard, .. } =
            self.descend_from(key, undo.and_then(|u| u.from))?;
        if let Some(written) =
            self.fault_unless_resident(key, (!separate).then_some(expect), leaf_frame, &guard)?
        {
            self.retried(RetrySite::EmptyingWritten);
            self.refused_by_writeback(written);
            return Err(Stop::Retry);
        }
        let ((at, hit), blob_checked) = self.blob_expectation(key, expect, leaf_frame, &guard)?;
        if blob_checked == Some(false) {
            guard.recheck()?;
            return Ok(Attempt::Mismatch);
        }
        let mut leaf = guard.upgrade().inspect_err(|_| Self::refused_by(leaf_latch))?;
        let (old_entry, old_blob) = {
            let node = NodeRef::held(&leaf);
            let flags = if hit { node.slot(at)?.4 } else { 0 };
            let old_entry = if flags & SEPARATED != 0 {
                Some(Separated::read(&node, at)?.ok_or(Stop::Refuse(Error::Corrupt))?)
            } else {
                None
            };
            let old_blob = if flags & BLOB != 0 {
                Some(Blob::read(&node, at)?.ok_or(Stop::Refuse(Error::Corrupt))?)
            } else {
                None
            };
            (old_entry, old_blob)
        };
        let old_page = old_entry
            .as_ref()
            .map(|e| PageId::new(e.page).map_err(|_| Stop::Refuse(Error::Corrupt)))
            .transpose()?;
        let local = if let Some(staged) = staged {
            staged.local
        } else if separate {
            crate::node::separated::local_len(self.keep_local, value.len())
        } else {
            0
        };
        let rest = &value[local..];
        let mut old = None;
        if let Some(page) = old_page {
            let Some(held) = self.value_latched(page)? else {
                drop(leaf);
                return self.refault(leaf_latch, leaf_frame, page);
            };
            old = Some(held);
        }
        let old_entries = if let Some(h) = &old {
            let page = NodeRef::held(&h.guard);
            if page.kind()? != PageKind::Value as u8 {
                return Err(Stop::Refuse(Error::Corrupt));
            }
            let (owner_at, owned) = page.lower_bound(key)?;
            if !owned {
                return Err(Stop::Refuse(Error::Corrupt));
            }
            let kept = old_entry.as_ref().map_or(&[][..], Separated::local);
            if !expect.matches_split(kept, &page, owner_at)? {
                return Ok(Attempt::Mismatch);
            }
            page.len()?
        } else if let Some((reference, kept)) = &old_blob {
            if blob_checked.is_none() {
                let miss = super::blob::OnMiss::Report;
                match self.blob_matches(expect, *reference, kept.bytes(), &miss)? {
                    Ok(true) => {}
                    Ok(false) => return Ok(Attempt::Mismatch),
                    Err(page) => {
                        drop(leaf);
                        return self.refault(leaf_latch, leaf_frame, page);
                    }
                }
            }
            0
        } else {
            let node = NodeRef::held(&leaf);
            if !expect.matches_entry(hit.then_some((&node, at)))? {
                return Ok(Attempt::Mismatch);
            }
            0
        };
        let entry_len = if staged.is_some() {
            crate::node::blob::REFERENCE_BYTES + local
        } else if separate {
            crate::node::separated::REFERENCE_BYTES + local
        } else {
            value.len()
        };
        if leaf.get().room_parts(key, b"", entry_len).is_none() {
            return Ok(Attempt::Full);
        }
        let mut old_free = None;
        let mut old_op = None;
        if separate
            && let Some(h) = old.as_mut()
            && let Some((free, added)) = Self::place_ranged(h, key, rest)
        {
            old_free = Some(free);
            old_op = Some(super::Op::put(added, key, rest, 0));
        }
        let in_place = old_free.is_some();
        let mut target: Option<Held<'_>> = None;
        let mut target_page = None;
        if separate {
            if in_place {
                target_page = old_page;
            } else if let Some(f) = fresh.as_ref() {
                target_page = f.child().page_id();
            } else if let Some(page) = hinted.then(|| self.room.candidate()).flatten() {
                let held = (Some(page) != old_page)
                    .then(|| self.value_latched(page))
                    .transpose()?
                    .flatten();
                let usable = held.as_ref().is_some_and(|h| {
                    let n = h.guard.get();
                    n.kind() == PageKind::Value as u8 && n.has_room_for(key, rest)
                });
                if !usable {
                    self.room.forget(page);
                    self.retried(RetrySite::HintUnusable);
                    return Err(Stop::Retry);
                }
                target_page = Some(page);
                target = held;
            }
        }
        if separate && target_page.is_none() {
            return Ok(Attempt::NeedsPage);
        }
        let reference = target_page.map(|p| crate::node::separated::encode(p.get(), value, local));
        let blob_entry = staged.map(|s| crate::node::blob::encode(s.reference, value, local));
        let (payload, flags): (&[u8], u16) = match (&blob_entry, &reference) {
            (Some((entry, n)), _) => (&entry[..*n], BLOB),
            (None, Some((entry, n))) => (&entry[..*n], SEPARATED),
            (None, None) => (value, 0),
        };
        let leaf_changes =
            !in_place || old_entry.as_ref().is_none_or(|e| e.local() != &value[..local]);
        debug_assert!(
            !leaf_changes || leaf.get().has_room_for(key, payload),
            "the leaf's room was settled before any value page was written"
        );
        let frees_old = old.is_some() && !in_place;
        let empties_old = frees_old && old_entries == 1;
        if empties_old && !old.as_ref().is_some_and(|h| self.no_writeback_in_flight(h.frame)) {
            self.retried(RetrySite::EmptyingWritten);
            if let Some(h) = old.as_ref() {
                self.refused_by_writeback(h.frame);
            }
            return Err(Stop::Retry);
        }
        let mut target_free = None;
        let mut target_op = None;
        if let Some(h) = target.as_mut() {
            let placed = Self::place_ranged(h, key, rest);
            debug_assert!(placed.is_some(), "a checked placement did not fit");
            target_free = placed.map(|(free, _)| free);
            target_op = placed.map(|(_, added)| super::Op::put(added, key, rest, 0));
        }
        if frees_old && let Some(h) = old.as_mut() {
            old_free = Some(h.guard.update(|n: &mut Node| {
                let freed = free(n, key);
                debug_assert_eq!(freed, Ok(true));
                n.free_space_after_compaction()
            }));
            old_op = Some(super::Op::remove(key));
        }
        let leaf_op = leaf_changes.then(|| {
            let added = leaf.update_ranges(|n: &mut Node| {
                let before = n.extent();
                let (inserted, compacted) = n.insert_reporting(key, payload, flags);
                debug_assert!(inserted);
                (n.len() > before.0, n.dirty_ranges(before, compacted))
            });
            super::Op::put(added, key, payload, flags)
        });
        let leaf_logged = leaf_op.as_ref().map(|op| self.logged_op(leaf_frame, &leaf, op));
        let placed_fresh = separate && !in_place && fresh.is_some();
        let fresh_image = if placed_fresh && self.wal.is_some() {
            fresh.as_ref().map(Fresh::image).transpose()?
        } else if placed_fresh {
            Some(Vec::new())
        } else {
            None
        };
        let target_logged = target
            .as_ref()
            .zip(target_op.as_ref())
            .map(|(h, op)| self.logged_op(h.frame, &h.guard, op));
        let old_logged =
            old.as_ref().zip(old_op.as_ref()).map(|(h, op)| self.logged_op(h.frame, &h.guard, op));
        let leaf_image = || Self::image_of(&leaf);
        let target_image = || target.as_ref().map_or_else(Vec::new, |h| Self::image_of(&h.guard));
        let old_image = || old.as_ref().map_or_else(Vec::new, |h| Self::image_of(&h.guard));
        let part = super::Part::logged;
        let mut pages = [super::Part::image(0, &[]); 4];
        let mut changed = 0;
        let named = [
            leaf_logged.as_ref().map(|l| part(leaf_frame, l, &leaf_image)),
            fresh
                .as_ref()
                .zip(fresh_image.as_deref())
                .map(|(f, i)| super::Part::image(f.frame(), i)),
            target
                .as_ref()
                .zip(target_logged.as_ref())
                .map(|(h, l)| part(h.frame, l, &target_image)),
            old.as_ref().zip(old_logged.as_ref()).map(|(h, l)| part(h.frame, l, &old_image)),
        ];
        for page in named.into_iter().flatten() {
            pages[changed] = page;
            changed += 1;
        }
        let mut unnamed = None;
        if staged.is_some() || old_blob.is_some() {
            let retiring = old_blob.as_ref().filter(|_| undo.is_none());
            let stamp = self.mark_publishing(
                &pages[..changed],
                undo,
                staged,
                retiring.map(|(r, k)| (*r, k.bytes().len())),
            );
            if let Some(staged) = staged {
                self.published(staged, stamp);
            }
            if let Some((reference, kept)) = &old_blob
                && undo.is_none()
            {
                unnamed = Some(self.retiring(*reference, kept.bytes().len(), Self::stamped(stamp)));
            }
        } else {
            match (&pages[..changed], undo) {
                (changed, Some(undo)) => self.mark_change(changed, undo),
                ([one], None) => self.mark_part(one),
                (changed, None) => self.mark_value_write(changed),
            }
        }
        if placed_fresh && let Some(f) = fresh.take() {
            let page = f.child().page_id();
            drop(f);
            if let Some(page) = page {
                self.room.opened(page);
            }
        }
        if let (Some(h), Some(free)) = (target.take(), target_free) {
            self.admit_if_roomy(h.page, free);
        }
        if let Some(h) = old.take() {
            if empties_old {
                let retired = self.retire_value_page(h.frame, h.page, &h.guard);
                debug_assert!(retired, "an emptied value page with no writeback was kept");
            } else if let Some(free) = old_free {
                self.admit_if_roomy(h.page, free);
            }
        }
        drop(leaf);
        if let Some(unnamed) = unnamed {
            self.retire_unnamed(unnamed);
        }
        if separate {
            self.values_placed.add(1);
        }
        if frees_old {
            self.value_relocations.add(1);
        }
        Ok(Attempt::Inserted)
    }
    fn fault_unless_resident(
        &self,
        key: &[u8],
        takes_entry: Option<Expect<'_>>,
        leaf_frame: u32,
        guard: &OptimisticGuard<'_, Node, NODE_WORDS>,
    ) -> Descent<Option<u32>> {
        let leaf = NodeRef::new(guard);
        let (at, hit) = leaf.lower_bound(key)?;
        if !hit {
            return Ok(None);
        }
        let (_, _, _, _, flags) = leaf.slot(at)?;
        if flags & crate::node::flags::SEPARATED == 0 {
            return Ok(None);
        }
        let Some(entry) = Separated::read(&leaf, at)? else { return Ok(None) };
        let Ok(page) = PageId::new(entry.page) else { return Ok(None) };
        if let Some((latch, frame, sample)) = self.predict(page)? {
            let Some(expect) = takes_entry else { return Ok(None) };
            if self.no_writeback_in_flight(frame) {
                return Ok(None);
            }
            let value = latch.optimistic().inspect_err(|_| Self::refused_by(latch))?;
            self.confirm(&super::Resolved { latch, page, frame, sample })?;
            let node = NodeRef::new(&value);
            let alone = node.len()? == 1;
            let (owner_at, owned) = node.lower_bound(key)?;
            let proceeds = owned && expect.matches_split(entry.local(), &node, owner_at)?;
            value.recheck()?;
            guard.recheck()?;
            return Ok((alone && proceeds).then_some(frame));
        }
        guard.recheck()?;
        self.value_write_refaults.add(1);
        self.retried(RetrySite::Refault);
        self.fault_value(page, leaf_frame, guard)?;
        Err(Stop::Retry)
    }
    fn refault<T>(
        &self,
        leaf: &crate::bufmgr::pool::PageLatch,
        frame: u32,
        page: PageId,
    ) -> Descent<T> {
        self.value_write_refaults.add(1);
        self.retried(RetrySite::Refault);
        let sample = leaf.optimistic().inspect_err(|_| Self::refused_by(leaf))?;
        self.fault_value_page(page, Route::Parent { frame, version: sample.sampled() })?;
        Err(Stop::Retry)
    }
    fn named_here(&self, frame: u32, page: PageId) -> bool {
        let Some(slot) = self.pool.slot(frame) else { return false };
        if !slot.frame.state().is_readable() || slot.frame.page_id() != Some(page) {
            return false;
        }
        for _ in 0..super::RETRY_BOUND {
            match self.translation.lookup(page) {
                Ok(named) => return named == Some(frame),
                Err(_) => crate::latch::spin(),
            }
        }
        false
    }
    fn admit_if_roomy(&self, page: PageId, free: usize) {
        if free > crate::node::DATA_BYTES / 2 {
            self.room.admit(page);
        }
    }
    fn place_ranged(held: &mut Held<'_>, key: &[u8], value: &[u8]) -> Option<(usize, bool)> {
        held.guard.update_ranges(|n: &mut Node| {
            let before = n.extent();
            match place_reporting(n, key, b"", value) {
                Ok((true, rewritten)) => (
                    Some((n.free_space_after_compaction(), n.len() > before.0)),
                    n.dirty_ranges(before, rewritten),
                ),
                _ => (None, [(0, 0); 2]),
            }
        })
    }
    pub(super) fn try_remove_value(&self, key: &[u8]) -> Descent<Option<(bool, crate::ChildRef)>> {
        let Leaf { child, frame: leaf_frame, latch: leaf_latch, guard, .. } = self.descend(key)?;
        if let Some(written) =
            self.fault_unless_resident(key, Some(Expect::Anything), leaf_frame, &guard)?
        {
            self.retried(RetrySite::EmptyingWritten);
            self.refused_by_writeback(written);
            return Err(Stop::Retry);
        }
        let mut leaf = guard.upgrade().inspect_err(|_| Self::refused_by(leaf_latch))?;
        let node = leaf.get();
        let (at, hit) = node.lower_bound(key);
        if hit && node.flags_at(at) & crate::node::flags::BLOB != 0 {
            let (reference, kept) =
                Blob::of(node.payload_at(at)).ok_or(Stop::Refuse(Error::Corrupt))?;
            let under_full = leaf.update(|n: &mut Node| {
                let removed = n.remove(key);
                debug_assert!(removed);
                n.is_under_full()
            });
            let removal = super::Op::remove(key);
            let logged = self.logged_op(leaf_frame, &leaf, &removal);
            let image = || Self::image_of(&leaf);
            let stamp = self.mark_publishing(
                &[super::Part::logged(leaf_frame, &logged, &image)],
                None,
                None,
                Some((reference, kept.bytes().len())),
            );
            let unnamed = self.retiring(reference, kept.bytes().len(), Self::stamped(stamp));
            drop(leaf);
            self.retire_unnamed(unnamed);
            return Ok(Some((under_full, child)));
        }
        if !hit || node.flags_at(at) & crate::node::flags::SEPARATED == 0 {
            self.retried(RetrySite::NoLongerSeparated);
            return Ok(None);
        }
        let (raw, _) = crate::node::separated::decode(node.payload_at(at))
            .ok_or(Stop::Refuse(Error::Corrupt))?;
        let page = PageId::new(raw).map_err(|_| Stop::Refuse(Error::Corrupt))?;
        let Some(mut old) = self.value_latched(page)? else {
            drop(leaf);
            return self.refault(leaf_latch, leaf_frame, page);
        };
        let old_node = old.guard.get();
        if old_node.kind() != PageKind::Value as u8 || read(&old_node, key)?.is_none() {
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let empties = old_node.len() == 1;
        if empties && !self.no_writeback_in_flight(old.frame) {
            self.retried(RetrySite::EmptyingWritten);
            self.refused_by_writeback(old.frame);
            return Err(Stop::Retry);
        }
        let free_after = old.guard.update(|n: &mut Node| {
            let freed = free(n, key);
            debug_assert_eq!(freed, Ok(true));
            n.free_space_after_compaction()
        });
        let under_full = leaf.update(|n: &mut Node| {
            let removed = n.remove(key);
            debug_assert!(removed);
            n.is_under_full()
        });
        let removal = super::Op::remove(key);
        let leaf_logged = self.logged_op(leaf_frame, &leaf, &removal);
        let old_logged = self.logged_op(old.frame, &old.guard, &removal);
        let (leaf_image, old_image) = (|| Self::image_of(&leaf), || Self::image_of(&old.guard));
        let part = super::Part::logged;
        self.mark_value_write(&[
            part(leaf_frame, &leaf_logged, &leaf_image),
            part(old.frame, &old_logged, &old_image),
        ]);
        if empties {
            let retired = self.retire_value_page(old.frame, old.page, &old.guard);
            debug_assert!(retired, "an emptied value page with no writeback was kept");
        } else {
            self.admit_if_roomy(old.page, free_after);
        }
        drop(old);
        drop(leaf);
        Ok(Some((under_full, child)))
    }
}
