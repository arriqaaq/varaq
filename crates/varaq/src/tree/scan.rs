use super::{Announced, Descent, ReadMode, Stop, Tree};
use crate::Error;
use crate::child::ChildRef;
use crate::linebuf::LineBuf;
use crate::node::reader::NodeRef;
use crate::node::{Node, PageKind};
#[derive(Default)]
pub struct Batch {
    bytes: LineBuf<u8>,
    ends: LineBuf<(u32, u32)>,
    prefix: LineBuf<u8>,
    slots: LineBuf<u8>,
    fence: LineBuf<u8>,
    skipped: usize,
    next: usize,
    entries: usize,
    absent: Option<Missing>,
    width: usize,
    budget: usize,
    most: usize,
    copy_most: usize,
    local: bool,
    leaf: Option<Box<Node>>,
    first: usize,
}
const LOCAL_INLINE: u8 = 0;
const LOCAL_SEPARATED: u8 = 1;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local<'a> {
    Inline(&'a [u8]),
    Separated(&'a [u8]),
}
const SUB_BATCH: usize = 8;
const FIRST_CHUNK: usize = 8;
#[derive(Default)]
struct ScanBuffers {
    batch: Batch,
    seek: LineBuf<u8>,
    next: LineBuf<u8>,
}
std::thread_local! {
    static SCAN_BUFFERS : core::cell::Cell < ScanBuffers > =
    core::cell::Cell::new(ScanBuffers::default());
}
struct Lent(ScanBuffers);
impl Lent {
    fn take() -> Self {
        Self(SCAN_BUFFERS.with(core::cell::Cell::take))
    }
}
impl Drop for Lent {
    fn drop(&mut self) {
        let mut buffers = core::mem::take(&mut self.0);
        if buffers.batch.bytes.capacity() > super::blob::KEPT_MOST {
            buffers.batch = Batch::default();
        }
        let _ = SCAN_BUFFERS.try_with(|slot| slot.set(buffers));
    }
}
impl Batch {
    #[inline]
    fn restart(&mut self, width: usize) {
        self.bytes.clear();
        self.ends.clear();
        self.prefix.clear();
        self.fence.clear();
        self.skipped = 0;
        self.next = 0;
        self.entries = 0;
        self.absent = None;
        self.width = width;
        self.budget = width;
    }
    #[inline]
    fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
        self.skipped = 0;
        self.absent = None;
        self.budget = self.width;
    }
    #[inline]
    fn widen(&mut self) {
        if self.width != usize::MAX {
            self.width = (self.width * 2).min(SUB_BATCH);
        }
    }
    #[inline]
    fn narrow(&mut self) {
        if self.width != usize::MAX {
            self.width = (self.width / 2).max(1);
        }
    }
    #[inline]
    fn len(&self) -> usize {
        self.ends.len()
    }
    #[inline]
    fn get(&self, i: usize) -> (&[u8], &[u8]) {
        let ends = self.ends.as_slice();
        let start = if i == 0 { 0 } else { ends[i - 1].1 as usize };
        let (key_end, payload_end) = ends[i];
        let bytes = self.bytes.as_slice();
        (&bytes[start..key_end as usize], &bytes[key_end as usize..payload_end as usize])
    }
    #[inline]
    fn absent_key(&self) -> &[u8] {
        let start = self.ends.as_slice().last().map_or(0, |e| e.1 as usize);
        &self.bytes.as_slice()[start..]
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Continue,
    Stop,
}
impl Tree {
    pub fn scan<F>(&self, from: &[u8], visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        self.walk(from, 0, true, usize::MAX, false, visit)
    }
    pub fn scan_local<F>(&self, from: &[u8], most: usize, mut visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], Local<'_>) -> Step,
    {
        self.walk(from, 0, true, most, true, |key, payload| match payload.split_first() {
            Some((&LOCAL_SEPARATED, kept)) => visit(key, Local::Separated(kept)),
            Some((_, bytes)) => visit(key, Local::Inline(bytes)),
            None => visit(key, Local::Inline(&[])),
        })
    }
    pub fn scan_prefixes<F>(&self, from: &[u8], len: usize, mut visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        self.walk(from, 0, true, len, false, |key, payload| {
            visit(key, &payload[..payload.len().min(len)])
        })
    }
    pub fn scan_skipping<F>(&self, from: &[u8], skip: usize, visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        self.walk(from, skip, true, usize::MAX, false, visit)
    }
    pub fn scan_keys_skipping<F>(&self, from: &[u8], skip: usize, mut visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Step,
    {
        self.walk(from, skip, false, 0, false, |key, _| visit(key))
    }
    pub fn scan_keys<F>(&self, from: &[u8], mut visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Step,
    {
        self.walk(from, 0, false, 0, false, |key, _| visit(key))
    }
    fn walk<F>(
        &self,
        from: &[u8],
        mut skip: usize,
        with_payload: bool,
        most: usize,
        local: bool,
        mut visit: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        self.refuse_if_poisoned()?;
        let mut lent = Lent::take();
        let ScanBuffers { batch, seek, next } = &mut lent.0;
        let (batch, mut seek, mut next) = (batch, seek, next);
        batch.restart(if with_payload && !local { 1 } else { usize::MAX });
        batch.most = most;
        batch.local = with_payload && local;
        batch.copy_most = match (with_payload, local) {
            (false, _) => 0,
            (true, true) => {
                most.max(crate::node::separated::PAYLOAD_MOST).max(crate::node::blob::PAYLOAD_MOST)
            }
            (true, false) => usize::MAX,
        };
        seek.clear();
        seek.extend_from_slice(from);
        next.clear();
        let mut leaves: u32 = 0;
        let mut hops = Tally(0, &self.scan_hops);
        let mut chunks = Tally(0, &self.scan_chunks);
        let mut refused = Tally(0, &self.scan_chunks_refused);
        let mut absent = Tally(0, &self.scan_absent);
        let mut absent_after_skip = Tally(0, &self.scan_absent_after_skip);
        let mut link: Option<u64> = None;
        let mut left: Option<(LeafGuard<'_>, u32)> = None;
        loop {
            leaves = leaves.saturating_add(1);
            let mut tries = 0_u32;
            let hopped = match link {
                Some(l) => self.retrying_read(|mode, _| {
                    tries += 1;
                    if tries > 1 {
                        batch.narrow();
                    }
                    match mode {
                        ReadMode::Optimistic => {
                            self.read_leaf_via(l, seek.as_slice(), skip, with_payload, batch)
                        }
                        ReadMode::Held => Ok(None),
                    }
                })?,
                None => None,
            };
            let hopped = hopped.filter(|_| left.as_ref().is_none_or(|(g, _)| g.recheck().is_ok()));
            let (fence, right, guard, frame) = match hopped {
                Some(v) => {
                    hops.0 += 1;
                    v
                }
                None => self.retrying_read(|mode, announced| {
                    tries += 1;
                    if tries > 1 {
                        batch.narrow();
                    }
                    self.read_one_leaf(seek.as_slice(), skip, with_payload, batch, mode, announced)
                })?,
            };
            left = Some((guard, frame));
            link = (right != 0).then_some(right);
            skip -= batch.skipped;
            if batch.local {
                if Self::visit_leaf(batch, &mut visit)? == Step::Stop {
                    return Ok(());
                }
            } else {
                for i in 0..batch.len() {
                    let (key, payload) = batch.get(i);
                    if visit(key, payload) == Step::Stop {
                        return Ok(());
                    }
                }
            }
            batch.widen();
            if let Some(missing) = batch.absent.take() {
                absent.0 += 1;
                absent_after_skip.0 += u64::from(batch.len() == 0 && batch.skipped > 0);
                resume_at_absent(next, batch);
                let (held, frame) = left.as_ref().ok_or(Error::Corrupt)?;
                if self.settle_missing(missing, (*frame, held), (batch, next), &mut visit)?
                    == Step::Stop
                {
                    return Ok(());
                }
                (seek, next) = (next, seek);
                (link, left) = (None, None);
                continue;
            }
            if batch.next < batch.entries {
                let (held, frame) = left.as_ref().ok_or(Error::Corrupt)?;
                match self.rest_of_leaf(held, with_payload, batch, next, &mut visit) {
                    (Rest::Stopped, copied) => {
                        chunks.0 += copied;
                        return Ok(());
                    }
                    (Rest::Drained, copied) => chunks.0 += copied,
                    (Rest::Failed(e), copied) => {
                        chunks.0 += copied;
                        return Err(e);
                    }
                    (Rest::Absent, copied) => {
                        chunks.0 += copied;
                        absent.0 += 1;
                        let missing = batch.absent.take().ok_or(Error::Corrupt)?;
                        if self.settle_missing(
                            missing,
                            (*frame, held),
                            (batch, next),
                            &mut visit,
                        )? == Step::Stop
                        {
                            return Ok(());
                        }
                        (seek, next) = (next, seek);
                        (link, left) = (None, None);
                        continue;
                    }
                    (Rest::Refused, copied) => {
                        chunks.0 += copied;
                        refused.0 += 1;
                        batch.narrow();
                        (seek, next) = (next, seek);
                        (link, left) = (None, None);
                        continue;
                    }
                }
            }
            if !fence {
                return Ok(());
            }
            next.clear();
            next.extend_from_slice(batch.fence.as_slice());
            next.push(0);
            if next.as_slice() <= seek.as_slice() {
                return Err(Error::Livelocked { attempts: leaves });
            }
            (seek, next) = (next, seek);
        }
    }
    fn read_one_leaf<'t>(
        &'t self,
        seek: &[u8],
        skip: usize,
        with_payload: bool,
        batch: &mut Batch,
        mode: ReadMode,
        announced: &mut Announced<'t>,
    ) -> Descent<(bool, u64, LeafGuard<'t>, u32)> {
        batch.clear();
        let super::Leaf { latch, guard, frame, .. } = match mode {
            ReadMode::Optimistic => self.descend(seek)?,
            ReadMode::Held => self.descend_trailed(seek, announced)?,
        };
        let held = match mode {
            ReadMode::Optimistic => None,
            ReadMode::Held => Some(guard.clone().try_shared().inspect_err(|_| {
                announced.on(latch);
                Self::refused_by(latch);
            })?),
        };
        let leaf = held.as_ref().map_or_else(|| NodeRef::new(&guard), NodeRef::shared);
        let (at, _) = leaf.lower_bound(seek)?;
        let n = leaf.len()?;
        batch.skipped = skip.min(n.saturating_sub(at));
        let at = at + batch.skipped;
        if at < n {
            leaf.prefix_into(&mut batch.prefix)?;
        }
        let end = if batch.local {
            let copy =
                batch.leaf.get_or_insert_with(|| Box::new(Node::new(PageKind::Leaf, b"", b"")));
            match &held {
                Some(h) => **copy = h.get(),
                None => **copy = guard.read()?,
            }
            batch.first = at;
            n
        } else if at < n {
            crate::calc::chunk_end(at, n, FIRST_CHUNK)
        } else {
            n
        };
        (batch.next, batch.entries) = (end, n);
        let copy_to = if batch.local { at } else { end };
        leaf.slots_into(at, copy_to, &mut batch.slots)?;
        for i in at..copy_to {
            let ends = leaf.entry_into(
                (batch.slots.as_slice(), i - at),
                batch.prefix.as_slice(),
                &mut batch.bytes,
                batch.copy_most,
            )?;
            let holding = held.is_some().then_some(&mut *announced);
            let Some(ends) = self.value_of(batch, ends, &guard, with_payload, holding)? else {
                batch.next = i;
                break;
            };
            batch.ends.push(ends);
            if batch.budget == 0 {
                batch.next = i + 1;
                break;
            }
        }
        leaf.upper_fence_into(&mut batch.fence)?;
        let right = leaf.right()?;
        if held.is_none() {
            guard.recheck()?;
        }
        drop(held);
        let _ = latch;
        Ok((!batch.fence.is_empty(), right, guard, frame))
    }
    fn read_leaf_via<'t>(
        &'t self,
        link: u64,
        seek: &[u8],
        skip: usize,
        with_payload: bool,
        batch: &mut Batch,
    ) -> Descent<Option<(bool, u64, LeafGuard<'t>, u32)>> {
        batch.clear();
        let child = ChildRef::from_raw(link);
        let Ok(resolved) = self.resolve(child) else { return Ok(None) };
        let guard =
            resolved.latch.optimistic().inspect_err(|_| Self::refused_by(resolved.latch))?;
        self.confirm(&resolved)?;
        let leaf = NodeRef::new(&guard);
        let (covers, from_start) =
            if leaf.is_leaf()? { leaf.covers_from_start(seek)? } else { (false, false) };
        if !covers {
            guard.recheck()?;
            return Ok(None);
        }
        let at = if from_start { 0 } else { leaf.lower_bound(seek)?.0 };
        let n = leaf.len()?;
        batch.skipped = skip.min(n.saturating_sub(at));
        let at = at + batch.skipped;
        if at < n {
            leaf.prefix_into(&mut batch.prefix)?;
        }
        let end = if batch.local {
            let copy =
                batch.leaf.get_or_insert_with(|| Box::new(Node::new(PageKind::Leaf, b"", b"")));
            **copy = guard.read()?;
            batch.first = at;
            n
        } else if at < n {
            crate::calc::chunk_end(at, n, FIRST_CHUNK)
        } else {
            n
        };
        (batch.next, batch.entries) = (end, n);
        let copy_to = if batch.local { at } else { end };
        leaf.slots_into(at, copy_to, &mut batch.slots)?;
        for i in at..copy_to {
            let ends = leaf.entry_into(
                (batch.slots.as_slice(), i - at),
                batch.prefix.as_slice(),
                &mut batch.bytes,
                batch.copy_most,
            )?;
            let Some(ends) = self.value_of(batch, ends, &guard, with_payload, None)? else {
                batch.next = i;
                break;
            };
            batch.ends.push(ends);
            if batch.budget == 0 {
                batch.next = i + 1;
                break;
            }
        }
        leaf.upper_fence_into(&mut batch.fence)?;
        let right = leaf.right()?;
        guard.recheck()?;
        Ok(Some((!batch.fence.is_empty(), right, guard, resolved.frame)))
    }
    #[cold]
    #[inline(never)]
    fn rest_of_leaf<F>(
        &self,
        guard: &LeafGuard<'_>,
        with_payload: bool,
        batch: &mut Batch,
        next: &mut LineBuf<u8>,
        visit: &mut F,
    ) -> (Rest, u64)
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        let mut chunk = FIRST_CHUNK;
        let mut copied = 0;
        while batch.next < batch.entries {
            next.clear();
            next.extend_from_slice(batch.get(batch.len() - 1).0);
            next.push(0);
            chunk = crate::calc::next_chunk(chunk);
            copied += 1;
            match self.copy_chunk(guard, chunk, with_payload, batch) {
                Ok(()) => {}
                Err(Stop::Refuse(e)) => return (Rest::Failed(e), copied),
                Err(_) => return (Rest::Refused, copied),
            }
            for i in 0..batch.len() {
                let (key, payload) = batch.get(i);
                if visit(key, payload) == Step::Stop {
                    return (Rest::Stopped, copied);
                }
            }
            batch.widen();
            if batch.absent.is_some() {
                resume_at_absent(next, batch);
                return (Rest::Absent, copied);
            }
        }
        (Rest::Drained, copied)
    }
    fn copy_chunk(
        &self,
        guard: &LeafGuard<'_>,
        chunk: usize,
        with_payload: bool,
        batch: &mut Batch,
    ) -> Descent<()> {
        batch.clear();
        let leaf = NodeRef::new(guard);
        let end = crate::calc::chunk_end(batch.next, batch.entries, chunk);
        let mut stopped = end;
        let from = batch.next;
        leaf.slots_into(from, end, &mut batch.slots)?;
        for i in from..end {
            let ends = leaf.entry_into(
                (batch.slots.as_slice(), i - from),
                batch.prefix.as_slice(),
                &mut batch.bytes,
                batch.copy_most,
            )?;
            let Some(ends) = self.value_of(batch, ends, guard, with_payload, None)? else {
                stopped = i;
                break;
            };
            batch.ends.push(ends);
            if batch.budget == 0 {
                stopped = i + 1;
                break;
            }
        }
        batch.next = stopped;
        Ok(guard.recheck()?)
    }
    fn value_of<'t>(
        &'t self,
        batch: &mut Batch,
        (key_end, payload_end, flags): (u32, u32, u16),
        guard: &LeafGuard<'_>,
        with_payload: bool,
        held: Option<&mut Announced<'t>>,
    ) -> Descent<Option<(u32, u32)>> {
        if !with_payload || flags & crate::node::flags::OUT_OF_LINE == 0 {
            return Ok(Some((key_end, payload_end)));
        }
        let start = batch.ends.as_slice().last().map_or(0, |e| e.1 as usize);
        let (key_end, payload_end) = (key_end as usize, payload_end as usize);
        if flags & crate::node::flags::BLOB != 0 {
            return Self::blob_of(batch, (key_end, payload_end), held.is_some());
        }
        let Some(entry) =
            super::value::Separated::of(&batch.bytes.as_slice()[key_end..payload_end])
        else {
            return Err(if held.is_some() { Stop::Refuse(Error::Corrupt) } else { Stop::Retry });
        };
        let room = batch.most.min(crate::PAGE_SIZE);
        batch.bytes.resize(key_end);
        batch.bytes.extend_unfilled(key_end + room);
        let (head, tail) = batch.bytes.as_mut_slice().split_at_mut(key_end);
        let value = match held {
            Some(announced) => {
                self.separated_held(entry.page, entry.local(), &head[start..], tail, announced)?
            }
            None => self.separated_into(entry.page, entry.local(), &head[start..], guard, tail)?,
        };
        let len = match value {
            super::value::Value::Read(len) => len,
            super::value::Value::Absent(page) => {
                batch.bytes.resize(key_end);
                batch.absent = Some(Missing::Page(page));
                return Ok(None);
            }
        };
        let end = key_end + len.min(room);
        batch.bytes.resize(end);
        batch.budget = batch.budget.saturating_sub(1);
        let (Ok(key_end), Ok(end)) = (u32::try_from(key_end), u32::try_from(end)) else {
            return Err(Stop::Retry);
        };
        Ok(Some((key_end, end)))
    }
}
impl Tree {
    fn blob_of(
        batch: &mut Batch,
        (key_end, payload_end): (usize, usize),
        held: bool,
    ) -> Descent<Option<(u32, u32)>> {
        let Some((reference, kept)) =
            super::value::Blob::of(&batch.bytes.as_slice()[key_end..payload_end])
        else {
            return Err(if held { Stop::Refuse(Error::Corrupt) } else { Stop::Retry });
        };
        batch.bytes.resize(key_end);
        batch.absent = Some(Missing::Blob(reference, kept));
        Ok(None)
    }
}
impl Tree {
    fn settle_missing<F>(
        &self,
        missing: Missing,
        (frame, leaf): (u32, &LeafGuard<'_>),
        (batch, next): (&mut Batch, &mut LineBuf<u8>),
        visit: &mut F,
    ) -> Result<Step, Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        match missing {
            Missing::Page(page) => {
                self.fault_scanned(page, frame, leaf)?;
                Ok(Step::Continue)
            }
            Missing::Blob(reference, kept) => {
                let mut room = Room { bytes: &mut batch.bytes, at: 0, most: batch.most };
                match self.blob_read(next.as_slice(), reference, kept.bytes(), &mut room) {
                    Ok(len) => {
                        let step =
                            visit(next.as_slice(), &batch.bytes.as_slice()[..len.min(batch.most)]);
                        next.push(0);
                        Ok(step)
                    }
                    Err(Stop::Refuse(e)) => Err(e),
                    Err(_) => Ok(Step::Continue),
                }
            }
        }
    }
}
struct Room<'b> {
    bytes: &'b mut crate::linebuf::LineBuf<u8>,
    at: usize,
    most: usize,
}
impl super::value::ValueSink for Room<'_> {
    fn sized(&mut self, len: usize) -> &mut [u8] {
        self.bytes.resize(self.at + len.min(self.most));
        &mut self.bytes.as_mut_slice()[self.at..]
    }
}
impl Tree {
    fn visit_leaf<F>(batch: &mut Batch, visit: &mut F) -> Result<Step, Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        let Batch { leaf, bytes, first, most, .. } = batch;
        let Some(node) = leaf.as_deref() else { return Ok(Step::Continue) };
        if *first >= node.len() {
            return Ok(Step::Continue);
        }
        if !node.is_well_formed() {
            return Err(Error::Corrupt);
        }
        let prefix = node.prefix();
        for i in *first..node.len() {
            bytes.clear();
            bytes.extend_from_slice(prefix);
            bytes.extend_from_slice(node.key_at(i));
            let key_end = bytes.len();
            let payload = node.payload_at(i);
            let flags = node.flags_at(i);
            if flags & crate::node::flags::OUT_OF_LINE == 0 {
                bytes.push(LOCAL_INLINE);
                bytes.extend_from_slice(&payload[..payload.len().min(*most)]);
            } else if flags & crate::node::flags::BLOB != 0 {
                let (_, kept) = crate::node::blob::decode(payload).ok_or(Error::Corrupt)?;
                bytes.push(LOCAL_SEPARATED);
                bytes.extend_from_slice(kept);
            } else {
                let (_, kept) = crate::node::separated::decode(payload).ok_or(Error::Corrupt)?;
                bytes.push(LOCAL_SEPARATED);
                bytes.extend_from_slice(kept);
            }
            let (key, rest) = bytes.as_slice().split_at(key_end);
            if visit(key, rest) == Step::Stop {
                return Ok(Step::Stop);
            }
        }
        Ok(Step::Continue)
    }
}
enum Missing {
    Page(crate::PageId),
    Blob(crate::node::blob::Ref, super::value::Kept),
}
enum Rest {
    Stopped,
    Drained,
    Refused,
    Absent,
    Failed(Error),
}
struct Tally<'t>(u64, &'t crate::stats::Stat);
impl Drop for Tally<'_> {
    fn drop(&mut self) {
        if self.0 > 0 {
            self.1.add(self.0);
        }
    }
}
type LeafGuard<'t> = crate::OptimisticGuard<'t, Node, { super::NODE_WORDS }>;
fn resume_at_absent(next: &mut LineBuf<u8>, batch: &Batch) {
    next.clear();
    next.extend_from_slice(batch.absent_key());
}
impl Tree {
    fn fault_scanned(
        &self,
        page: crate::PageId,
        frame: u32,
        leaf: &LeafGuard<'_>,
    ) -> Result<(), Error> {
        match self.fault_value(page, frame, leaf) {
            Err(Stop::Refuse(e)) => Err(e),
            _ => Ok(()),
        }
    }
}
