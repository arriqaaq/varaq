use crate::wal::record::{self, Header, Kind, payload};
use crate::wal::scan;
use crate::{Error, PageId};
use std::collections::HashMap;
use std::sync::Arc;
pub const SCAN_WINDOW: usize = 16 << 20;
const REDO_WINDOW: usize = 1 << 20;
pub trait Rings: Send + Sync {
    fn capacity(&self, worker: usize) -> u64;
    fn read(&self, worker: usize, at: u64, into: &mut [u8]) -> Result<(), Error>;
    fn ended(&self, worker: usize, end: u64) -> Result<(), Error> {
        let _ = (worker, end);
        Ok(())
    }
}
impl Rings for crate::wal::worker::Wal {
    fn capacity(&self, _worker: usize) -> u64 {
        self.ring_device_bytes()
    }
    fn read(&self, worker: usize, at: u64, into: &mut [u8]) -> Result<(), Error> {
        self.read_ring(worker, at, into)
    }
}
pub struct WriteBackAsRead {
    wal: Arc<crate::wal::worker::Wal>,
    held: std::sync::Mutex<Held>,
    ended: core::sync::atomic::AtomicUsize,
}
#[derive(Default)]
struct Held {
    window: Option<(usize, u64, usize)>,
    bytes: Vec<u8>,
}
impl WriteBackAsRead {
    #[must_use]
    pub fn new(wal: Arc<crate::wal::worker::Wal>) -> Self {
        Self {
            wal,
            held: std::sync::Mutex::new(Held::default()),
            ended: core::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn done(&self) -> bool {
        self.ended.load(core::sync::atomic::Ordering::Acquire) >= self.wal.workers()
    }
    fn write(&self, worker: usize, start: u64, bytes: &[u8], upto: u64) -> Result<(), Error> {
        let n = usize::try_from(upto.div_ceil(crate::PAGE_SIZE as u64) * crate::PAGE_SIZE as u64)
            .map_err(|_| Error::Corrupt)?
            .min(bytes.len());
        if n == 0 { Ok(()) } else { self.wal.write_back(worker, start, &bytes[..n]) }
    }
}
impl Rings for WriteBackAsRead {
    fn capacity(&self, _worker: usize) -> u64 {
        self.wal.ring_device_bytes()
    }
    fn read(&self, worker: usize, at: u64, into: &mut [u8]) -> Result<(), Error> {
        self.wal.read_ring(worker, at, into)?;
        if self.done() {
            return Ok(());
        }
        let mut held = self.held.lock().map_err(|_| Error::Corrupt)?;
        let Held { window, bytes } = &mut *held;
        if let Some((w, start, n)) = window.take() {
            let upto = if w == worker && at >= start { at - start } else { n as u64 };
            self.write(w, start, &bytes[..n], upto)?;
        }
        bytes.clear();
        bytes.extend_from_slice(into);
        *window = Some((worker, at, into.len()));
        Ok(())
    }
    fn ended(&self, worker: usize, end: u64) -> Result<(), Error> {
        let len = self.wal.ring_device_bytes();
        let mut held = self.held.lock().map_err(|_| Error::Corrupt)?;
        let Held { window, bytes } = &mut *held;
        if let Some((w, start, n)) = window.take_if(|(w, _, _)| *w == worker) {
            let off = if len == 0 { 0 } else { end % len };
            let upto = if off >= start { off - start } else { n as u64 };
            self.write(w, start, &bytes[..n], upto)?;
        }
        self.ended.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
}
#[derive(Clone, Default)]
pub struct Log(Option<Arc<dyn Rings>>);
impl core::fmt::Debug for Log {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(if self.0.is_some() { "Log(rings)" } else { "Log(none)" })
    }
}
impl Log {
    fn rings(&self) -> Result<&dyn Rings, Error> {
        self.0.as_deref().ok_or(Error::RecoveryFailed)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Logged {
    pub at: u64,
    pub gsn: u64,
    pub txn: u64,
    pub len: u32,
    pub segment: u32,
    pub kind: Kind,
}
#[derive(Debug, Clone, Default)]
pub struct Ring {
    pub logged: Vec<Logged>,
    pub records: usize,
    pub stop: scan::Stop,
    pub end: u64,
    pub last_gsn: u64,
    pub begin: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Abandoned,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AfterRedo {
    pub stamp: u64,
    pub absorbed: bool,
}
impl AfterRedo {
    #[must_use]
    pub fn of(page: &[u8; crate::PAGE_SIZE]) -> Self {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&page[crate::node::offsets::GSN..crate::node::offsets::GSN + 8]);
        Self { stamp: u64::from_le_bytes(raw), absorbed: is_absorbed_image(page) }
    }
}
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub rings: Vec<Ring>,
    pub outcomes: HashMap<(u16, u32, u64), Outcome>,
    pub requires: HashMap<(u16, u32, u64), (u32, u32)>,
    pub required: Vec<(u64, u64)>,
    pub named: HashMap<(u16, u32, u64), Vec<(u16, u64, u64)>>,
    pub highest_gsn: u64,
    pub highest_page: u64,
    pub highest_blob_serial: u64,
    pub checkpoint_floor: u64,
    pub root_moves: Vec<RootMove>,
    pub highest_txn: u64,
    applies: Vec<Apply>,
    pub log: Log,
    pub after_redo: HashMap<u64, AfterRedo>,
    pub effective: std::collections::HashSet<(u16, u32)>,
    pub run_events: Vec<RunEvent>,
}
impl Analysis {
    pub fn named_pages(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.applies.iter().map(|a| (a.page, a.gsn))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootMove {
    pub gsn: u64,
    pub root: u64,
    pub pages: Vec<u64>,
}
#[must_use]
pub fn recovered_root(analysis: &Analysis, store: &dyn crate::store::PageStore) -> Option<u64> {
    let chains = chains(&analysis.applies);
    let mut buf = vec![0u8; crate::PAGE_SIZE];
    let mut carries = |page: u64, gsn: u64| {
        let Ok(pid) = PageId::new(page) else { return false };
        if store.read(pid, &mut buf).is_err() {
            return false;
        }
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[crate::node::offsets::GSN..crate::node::offsets::GSN + 8]);
        u64::from_le_bytes(raw) >= gsn && !skipped(chains.get(&page), gsn)
    };
    let mut moves: Vec<&RootMove> = analysis.root_moves.iter().collect();
    moves.sort_by_key(|m| core::cmp::Reverse(m.gsn));
    moves.into_iter().find(|m| m.pages.iter().all(|&p| carries(p, m.gsn))).map(|m| m.root)
}
pub fn analyse_log(rings: &Arc<dyn Rings>, starts: &[(u64, u64)]) -> Result<Analysis, Error> {
    let mut out = Analysis {
        rings: Vec::with_capacity(starts.len()),
        log: Log(Some(Arc::clone(rings))),
        ..Analysis::default()
    };
    for (worker, &(begin, lap)) in starts.iter().enumerate() {
        let len = rings.capacity(worker);
        let off = if len == 0 { 0 } else { begin % len };
        let read = |at: u64, into: &mut [u8]| rings.read(worker, at, into);
        let mut window = scan::Window::new(len, SCAN_WINDOW, &read);
        let mut reading = Reading::new(u16::try_from(worker).unwrap_or(u16::MAX), begin);
        let (first_stop, first) =
            scan::scan_window(&mut window, off, len, lap, 0, &mut |h, at, body| {
                reading.take(&mut out, h, begin + (at - off), body);
            })?;
        let mut consumed = first;
        let mut stop = first_stop;
        let to_end = len - off;
        let at_end = matches!(first_stop, scan::Stop::Exhausted)
            || (to_end - first < record::HEADER_BYTES as u64
                && matches!(first_stop, scan::Stop::TooShort | scan::Stop::NotARecord));
        if at_end && to_end < len {
            let from_gsn = reading.last_moving_gsn;
            let (second_stop, second) =
                scan::scan_window(&mut window, 0, off, lap + 1, from_gsn, &mut |h, at, body| {
                    reading.take(&mut out, h, begin + to_end + at, body);
                })?;
            consumed = to_end + second;
            stop = second_stop;
        }
        let mut ring = reading.ring;
        ring.stop = stop;
        ring.end = begin + consumed;
        rings.ended(worker, ring.end)?;
        out.rings.push(ring);
    }
    out.applies.sort_by_key(|a| a.gsn);
    out.applies.shrink_to_fit();
    out.required.shrink_to_fit();
    for ring in &mut out.rings {
        ring.logged.shrink_to_fit();
    }
    Ok(out)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEvent {
    pub worker: u16,
    pub record: u32,
    pub gsn: u64,
    pub first: u64,
    pub pages: u64,
    pub became: crate::tree::space::Became,
}
impl Analysis {
    #[must_use]
    pub fn blob_events(&self) -> Vec<(u64, u64, u64, crate::tree::space::Became)> {
        self.run_events
            .iter()
            .filter(|e| self.effective.contains(&(e.worker, e.record)))
            .map(|e| (e.first, e.pages, e.gsn, e.became))
            .collect()
    }
}
struct Reading {
    worker: u16,
    ring: Ring,
    segment: u32,
    last_moving_gsn: u64,
    own: Vec<(u64, u64)>,
    owner: Option<u64>,
}
impl Reading {
    fn new(worker: u16, begin: u64) -> Self {
        Self {
            worker,
            ring: Ring { begin, ..Ring::default() },
            segment: 0,
            last_moving_gsn: 0,
            own: Vec::new(),
            owner: None,
        }
    }
    fn take(&mut self, out: &mut Analysis, header: &Header, at: u64, body: &[u8]) {
        let worker = self.worker;
        self.ring.records += 1;
        if header.kind != Kind::CarriageReturn {
            self.last_moving_gsn = header.gsn;
        }
        out.highest_gsn = out.highest_gsn.max(header.gsn);
        out.highest_txn = out.highest_txn.max(header.txn);
        self.ring.last_gsn = self.ring.last_gsn.max(header.gsn);
        out.highest_page = out.highest_page.max(header.page);
        if header.kind == Kind::Smo && header.page != 0 {
            let pages = (0..payload::smo_pages(body).unwrap_or(0))
                .filter_map(|i| payload::read_smo(body, i).ok().map(|e| e.page))
                .collect();
            out.root_moves.push(RootMove { gsn: header.gsn, root: header.page, pages });
        }
        let entries = entries_of(header.kind, body);
        for (first, pages, serial) in runs_in(&entries) {
            out.highest_page = out.highest_page.max(first.saturating_add(pages - 1));
            out.highest_blob_serial = out.highest_blob_serial.max(serial);
        }
        let changes: Vec<(u64, u64, bool)> = match header.kind {
            Kind::Page => vec![(header.page, header.prev, false)],
            Kind::PageOp => vec![(header.page, header.prev, true)],
            _ => entries
                .iter()
                .filter(|(_, _, c)| !c.is_run())
                .map(|&(p, prev, c)| (p, prev, c.is_op()))
                .collect(),
        };
        if logs_a_page(header.kind) {
            let record = u32::try_from(self.ring.logged.len()).unwrap_or(u32::MAX);
            self.ring.logged.push(Logged {
                at,
                gsn: header.gsn,
                txn: header.txn,
                len: header.len,
                segment: self.segment,
                kind: header.kind,
            });
            let event = |first: u64, pages: u64, became| RunEvent {
                worker,
                record,
                gsn: header.gsn,
                first,
                pages,
                became,
            };
            for &(first, pages, change) in &entries {
                use crate::tree::space::Became;
                match change {
                    payload::Change::Run(_) => {
                        out.run_events.push(event(first, pages, Became::Named));
                    }
                    payload::Change::Retire(_) => {
                        out.run_events.push(event(first, pages, Became::Free));
                    }
                    _ => {}
                }
            }
            if header.kind == Kind::Change
                && let Ok(undo) = payload::read_change_undo(body)
                && let Some((reference, kept)) = undo.before.and_then(crate::node::blob::of_token)
            {
                let pages = crate::node::blob::pages_for(reference.rest(kept.len()));
                out.run_events.push(event(
                    reference.first,
                    pages,
                    crate::tree::space::Became::Free,
                ));
            }
            for (entry, &(page, prev, op)) in changes.iter().enumerate() {
                out.highest_page = out.highest_page.max(page);
                out.applies.push(Apply {
                    gsn: header.gsn,
                    page,
                    prev,
                    worker,
                    record,
                    entry: u16::try_from(entry).unwrap_or(u16::MAX),
                    op,
                });
            }
        }
        match header.kind {
            Kind::Change => {
                if self.owner != Some(header.txn) {
                    self.own.clear();
                    self.owner = Some(header.txn);
                }
                self.own.extend(changes.iter().map(|&(page, _, _)| (page, header.gsn)));
            }
            Kind::Commit => {
                let key = (worker, self.segment, header.txn);
                if self.owner != Some(header.txn) {
                    self.own.clear();
                }
                out.outcomes.insert(key, Outcome::Committed);
                if let Ok((_, named)) = payload::read_commit(body)
                    && !named.is_empty()
                {
                    out.named.insert(key, named);
                }
                self.owner = None;
                let from = u32::try_from(out.required.len()).unwrap_or(u32::MAX);
                let len = u32::try_from(self.own.len()).unwrap_or(u32::MAX);
                out.required.append(&mut self.own);
                out.requires.insert(key, (from, len));
            }
            Kind::Abort => {
                out.outcomes.insert((worker, self.segment, header.txn), Outcome::Abandoned);
                self.own.clear();
                self.owner = None;
                self.segment = self.segment.saturating_add(1);
            }
            _ => {}
        }
    }
}
const fn logs_a_page(kind: Kind) -> bool {
    matches!(kind, Kind::Page | Kind::PageOp | Kind::Smo | Kind::ValueWrite | Kind::Change)
}
pub fn honour_named(analysis: &mut Analysis) {
    loop {
        let mut dropped = Vec::new();
        for (key, named) in &analysis.named {
            if analysis.outcomes.get(key) != Some(&Outcome::Committed) {
                continue;
            }
            let lost = named.iter().any(|&(worker, tag, upto)| {
                let released =
                    analysis.rings.get(usize::from(worker)).is_none_or(|ring| upto <= ring.begin);
                !released
                    && !analysis.outcomes.iter().any(|(&(w, _, t), outcome)| {
                        w == worker && t == tag && *outcome == Outcome::Committed
                    })
            });
            if lost {
                dropped.push(*key);
            }
        }
        if dropped.is_empty() {
            return;
        }
        for key in dropped {
            analysis.outcomes.insert(key, Outcome::Abandoned);
        }
    }
}
pub fn decide(analysis: &mut Analysis, store: &dyn crate::store::PageStore) {
    let chains = chains(&analysis.applies);
    let mut buf = vec![0u8; crate::PAGE_SIZE];
    let mut carried: HashMap<u64, u64> = HashMap::new();
    for (key, &(from, len)) in &analysis.requires {
        let pages = analysis.required.get(from as usize..from as usize + len as usize);
        let complete = pages.is_some_and(|pages| {
            pages.iter().all(|&(page, gsn)| {
                let on_page = *carried.entry(page).or_insert_with(|| {
                    if let Some(after) = analysis.after_redo.get(&page) {
                        return after.stamp;
                    }
                    let Ok(pid) = PageId::new(page) else { return 0 };
                    if store.read(pid, &mut buf).is_err() {
                        return 0;
                    }
                    let mut raw = [0u8; 8];
                    raw.copy_from_slice(
                        &buf[crate::node::offsets::GSN..crate::node::offsets::GSN + 8],
                    );
                    u64::from_le_bytes(raw)
                });
                on_its_page(on_page, gsn, chains.get(&page))
            })
        });
        if !complete {
            analysis.outcomes.insert(*key, Outcome::Abandoned);
        }
    }
}
pub const REDO_READ_DEPTH: usize = 32;
struct FirstReads<'a> {
    channel: &'a dyn crate::store::channel::IoChannel,
    depth: usize,
    order: Vec<PageId>,
    next: usize,
    inflight: std::collections::HashSet<u64>,
    staged: HashMap<u64, (crate::store::channel::PageBuf, Result<(), Error>)>,
}
impl<'a> FirstReads<'a> {
    fn new(
        channel: &'a dyn crate::store::channel::IoChannel,
        depth: usize,
        queue: &[Apply],
        on_device: u64,
    ) -> Self {
        let mut seen = std::collections::HashSet::new();
        let order = queue
            .iter()
            .filter(|a| a.page <= on_device && seen.insert(a.page))
            .filter_map(|a| PageId::new(a.page).ok())
            .collect();
        let mut reads = Self {
            channel,
            depth: depth.max(1),
            order,
            next: 0,
            inflight: std::collections::HashSet::new(),
            staged: HashMap::new(),
        };
        reads.top_up();
        reads
    }
    fn top_up(&mut self) {
        while self.inflight.len() + self.staged.len() < self.depth && self.next < self.order.len() {
            let page = self.order[self.next];
            if self.channel.push_read(page, vec![0u8; crate::PAGE_SIZE].into_boxed_slice()).is_err()
            {
                break;
            }
            self.inflight.insert(page.get());
            self.next += 1;
        }
        let _ = self.channel.submit();
    }
    fn read(
        &mut self,
        store: &dyn crate::store::PageStore,
        pid: PageId,
        buf: &mut [u8],
    ) -> Result<Result<(), Error>, Error> {
        loop {
            if let Some((bytes, outcome)) = self.staged.remove(&pid.get()) {
                let outcome = outcome.and_then(|()| {
                    if bytes.len() != buf.len() {
                        return Err(Error::PageCorrupt { pid, stored: 0, computed: 0 });
                    }
                    crate::page::checksum::verify(&bytes, pid)
                });
                if outcome.is_ok() {
                    buf.copy_from_slice(&bytes);
                }
                self.top_up();
                return Ok(outcome);
            }
            if !self.inflight.contains(&pid.get()) {
                let outcome = store.read(pid, buf);
                self.top_up();
                return Ok(outcome);
            }
            for c in self.channel.poll(1)? {
                self.inflight.remove(&c.page.get());
                self.staged.insert(c.page.get(), (c.buf, c.outcome));
            }
        }
    }
}
impl Drop for FirstReads<'_> {
    fn drop(&mut self) {
        while !self.inflight.is_empty() {
            match self.channel.poll(1) {
                Ok(done) if !done.is_empty() => {
                    for c in done {
                        self.inflight.remove(&c.page.get());
                    }
                }
                _ => break,
            }
        }
    }
}
#[derive(Debug, Clone, Copy)]
struct Apply {
    gsn: u64,
    page: u64,
    prev: u64,
    worker: u16,
    record: u32,
    entry: u16,
    op: bool,
}
type Read<'a> = Box<dyn Fn(u64, &mut [u8]) -> Result<(), Error> + 'a>;
struct Payloads<'a> {
    rings: &'a dyn Rings,
    windows: Vec<scan::Window<'a>>,
}
impl<'a> Payloads<'a> {
    fn new(rings: &'a dyn Rings, reads: &'a [Read<'a>], size: usize) -> Self {
        let windows = reads
            .iter()
            .enumerate()
            .map(|(worker, read)| scan::Window::new(rings.capacity(worker), size, read.as_ref()))
            .collect();
        Self { rings, windows }
    }
    fn reads(rings: &'a dyn Rings, workers: usize) -> Vec<Read<'a>> {
        (0..workers)
            .map(|worker| {
                Box::new(move |at: u64, into: &mut [u8]| rings.read(worker, at, into)) as Read<'a>
            })
            .collect()
    }
    fn payload(&mut self, worker: usize, logged: &Logged) -> Result<&[u8], Error> {
        let len = self.rings.capacity(worker);
        let window = self.windows.get_mut(worker).ok_or(Error::RecoveryFailed)?;
        if len == 0 {
            return Err(Error::RecoveryFailed);
        }
        let at = logged.at % len;
        let lap = logged.at / len;
        let Some((header, body)) = window.record(at, len)? else {
            return Err(Error::RecoveryFailed);
        };
        if header.kind != logged.kind
            || header.gsn != logged.gsn
            || header.txn != logged.txn
            || header.len != logged.len
            || header.lap != lap
        {
            return Err(Error::RecoveryFailed);
        }
        Ok(body)
    }
}
fn pages_of(kind: Kind, body: &[u8]) -> Vec<(u64, u64, payload::Change<'_>)> {
    let mut pages = entries_of(kind, body);
    pages.retain(|(_, _, change)| !change.is_run());
    pages
}
fn runs_in<'e>(
    entries: &'e [(u64, u64, payload::Change<'_>)],
) -> impl Iterator<Item = (u64, u64, u64)> + 'e {
    entries.iter().filter_map(|&(first, pages, change)| match change {
        payload::Change::Run(serial) => {
            let raw: [u8; 8] = serial.try_into().ok()?;
            (pages > 0).then_some((first, pages, u64::from_le_bytes(raw)))
        }
        _ => None,
    })
}
fn entries_of(kind: Kind, body: &[u8]) -> Vec<(u64, u64, payload::Change<'_>)> {
    match kind {
        Kind::Smo => (0..payload::smo_pages(body).unwrap_or(0))
            .filter_map(|i| payload::read_smo(body, i).ok())
            .map(|e| (e.page, e.prev, payload::Change::Image(e.image)))
            .collect(),
        Kind::ValueWrite => (0..payload::value_write_pages(body).unwrap_or(0))
            .filter_map(|i| payload::read_value_write(body, i).ok())
            .map(|e| (e.page, e.prev, e.change))
            .collect(),
        Kind::Change => (0..payload::change_pages(body).unwrap_or(0))
            .filter_map(|i| payload::read_change(body, i).ok())
            .map(|e| (e.page, e.prev, e.change))
            .collect(),
        _ => Vec::new(),
    }
}
#[must_use]
pub fn is_absorbed_image(page: &[u8; crate::PAGE_SIZE]) -> bool {
    let count = u16::from_ne_bytes([
        page[crate::node::offsets::COUNT],
        page[crate::node::offsets::COUNT + 1],
    ]);
    if count != 0 {
        return false;
    }
    let node = crate::node::Node::from_page_bytes(page);
    let mut empty = crate::node::Node::empty(node.kind(), &[], &[]);
    empty.set_gsn(node.gsn());
    let expected = empty.to_page_bytes();
    let stamped = |at: usize| payload::STAMPED.iter().any(|&(from, to)| (from..to).contains(&at));
    page.iter().zip(expected.iter()).enumerate().all(|(at, (a, b))| a == b || stamped(at))
}
#[cfg(feature = "stats")]
pub static TORN_REBUILT_WITH_CHANGES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub fn redo(analysis: &Analysis, store: &dyn crate::store::PageStore) -> Result<usize, Error> {
    redo_reporting(analysis, store).map(|(applied, _, _)| applied)
}
pub type Redone = (usize, HashMap<u64, AfterRedo>, std::collections::HashSet<(u16, u32)>);
pub fn redo_reporting(
    analysis: &Analysis,
    store: &dyn crate::store::PageStore,
) -> Result<Redone, Error> {
    redo_with(analysis, store, None)
}
pub fn redo_reading(
    analysis: &Analysis,
    store: &dyn crate::store::PageStore,
    channel: &dyn crate::store::channel::IoChannel,
) -> Result<Redone, Error> {
    redo_with(analysis, store, Some((channel, REDO_READ_DEPTH)))
}
const REDO_PENDING_PAGES: usize = 4_096;
#[derive(Default)]
struct Pending {
    images: HashMap<u64, Box<[u8; crate::PAGE_SIZE]>>,
    order: std::collections::VecDeque<u64>,
}
impl Pending {
    fn holds(&self, page: u64) -> bool {
        self.images.contains_key(&page)
    }
    fn read(&self, page: u64, into: &mut [u8]) -> bool {
        match self.images.get(&page) {
            Some(image) => {
                into[..crate::PAGE_SIZE].copy_from_slice(&image[..]);
                true
            }
            None => false,
        }
    }
    fn put(
        &mut self,
        store: &dyn crate::store::PageStore,
        pid: PageId,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let image = <&[u8; crate::PAGE_SIZE]>::try_from(&bytes[..crate::PAGE_SIZE])
            .map_err(|_| Error::Corrupt)?;
        if let Some(held) = self.images.get_mut(&pid.get()) {
            held.copy_from_slice(image);
            return Ok(());
        }
        if self.images.len() >= REDO_PENDING_PAGES
            && let Some(oldest) = self.order.pop_front()
            && let Some(held) = self.images.remove(&oldest)
        {
            store.write(PageId::new(oldest)?, &held[..])?;
        }
        self.images.insert(pid.get(), Box::new(*image));
        self.order.push_back(pid.get());
        Ok(())
    }
    fn write_all(&mut self, store: &dyn crate::store::PageStore) -> Result<(), Error> {
        let mut held: Vec<_> = self.images.drain().collect();
        held.sort_unstable_by_key(|(page, _)| *page);
        self.order.clear();
        for (page, image) in held {
            store.write(PageId::new(page)?, &image[..])?;
        }
        Ok(())
    }
}
fn redo_with(
    analysis: &Analysis,
    store: &dyn crate::store::PageStore,
    reading: Option<(&dyn crate::store::channel::IoChannel, usize)>,
) -> Result<Redone, Error> {
    let on_device = u64::try_from(store.len()).unwrap_or(u64::MAX);
    while u64::try_from(store.len()).unwrap_or(u64::MAX) < analysis.highest_page {
        store.allocate()?;
    }
    let queue = &analysis.applies;
    let mut first_reads =
        reading.map(|(channel, depth)| FirstReads::new(channel, depth, queue, on_device));
    let chains = chains(queue);
    let images = images(queue);
    let rings = analysis.log.rings()?;
    let reads = Payloads::reads(rings, analysis.rings.len());
    let mut payloads = Payloads::new(rings, &reads, REDO_WINDOW);
    let mut carried: HashMap<u64, Base> = HashMap::new();
    let mut on_device_now: HashMap<u64, AfterRedo> = HashMap::new();
    let mut torn_based: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut read_whole: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut wrote: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut applied = 0usize;
    let mut effective = std::collections::HashSet::new();
    let mut buf = vec![0u8; crate::PAGE_SIZE];
    let mut pages = vec![vec![0u8; crate::PAGE_SIZE]; payload::SMO_MAX_PAGES];
    let mut pending = Pending::default();
    let mut at = 0;
    while at < queue.len() {
        let (worker, record) = (queue[at].worker, queue[at].record);
        let end = queue[at..]
            .iter()
            .position(|a| (a.worker, a.record) != (worker, record))
            .map_or(queue.len(), |n| at + n);
        let group = &queue[at..end];
        at = end;
        if group.len() > payload::SMO_MAX_PAGES {
            return Err(Error::RecoveryFailed);
        }
        let mut standing = [(false, false); payload::SMO_MAX_PAGES];
        for (item, stands) in group.iter().zip(standing.iter_mut()) {
            let base = if let Some(&known) = carried.get(&item.page) {
                known
            } else {
                let read = 'base: {
                    on_device_now.insert(item.page, AfterRedo::default());
                    let Ok(pid) = PageId::new(item.page) else { break 'base Base::Unwritten };
                    if item.page > on_device {
                        break 'base Base::Unwritten;
                    }
                    let outcome = match first_reads.as_mut() {
                        Some(reads) => reads.read(store, pid, &mut buf)?,
                        None => store.read(pid, &mut buf),
                    };
                    match outcome {
                        Ok(()) => {}
                        Err(Error::PageUnwritten { .. }) => break 'base Base::Unwritten,
                        Err(_) => {
                            let start = torn_start(
                                chains.get(&item.page).map(|c| &c.records),
                                images.get(&item.page),
                                analysis.checkpoint_floor,
                            );
                            if start.is_some() {
                                torn_based.insert(item.page);
                            }
                            break 'base start.map_or(Base::Torn, Base::At);
                        }
                    }
                    let image = <&[u8; crate::PAGE_SIZE]>::try_from(buf.as_slice())
                        .map_err(|_| Error::Corrupt)?;
                    let after = AfterRedo::of(image);
                    let read = after.stamp;
                    on_device_now.insert(item.page, after);
                    read_whole.insert(item.page);
                    Base::At(read)
                };
                carried.insert(item.page, read);
                read
            };
            let next = is_next(base, item.prev);
            let past = base
                .stamp()
                .is_some_and(|on_page| on_its_page(on_page, item.gsn, chains.get(&item.page)));
            *stands = (next, past);
        }
        if !group_applies(&standing[..group.len()]) {
            if standing[..group.len()].iter().all(|&(_, past)| past) {
                effective.insert((worker, record));
            }
            continue;
        }
        let w = usize::from(worker);
        let logged = analysis
            .rings
            .get(w)
            .and_then(|r| r.logged.get(record as usize))
            .ok_or(Error::RecoveryFailed)?;
        let body = payloads.payload(w, logged)?;
        let carried_by = match logged.kind {
            Kind::Page => vec![payload::Change::Image(body)],
            Kind::PageOp => vec![payload::Change::Op(body)],
            kind => pages_of(kind, body).into_iter().map(|(_, _, change)| change).collect(),
        };
        let changes: Vec<payload::Change<'_>> = group
            .iter()
            .map(|item| carried_by.get(usize::from(item.entry)).copied())
            .collect::<Option<_>>()
            .ok_or(Error::RecoveryFailed)?;
        let mut unreadable = false;
        for ((item, change), &(next, _)) in group.iter().zip(&changes).zip(&standing) {
            let readable = match *change {
                payload::Change::Image(bytes) => payload::read_image(bytes).is_ok(),
                payload::Change::Op(bytes) => {
                    payload::read_op(bytes).is_ok()
                        && (!next
                            || pending.holds(item.page)
                            || PageId::new(item.page)
                                .is_ok_and(|pid| store.read(pid, &mut buf).is_ok()))
                }
                payload::Change::Run(_) | payload::Change::Retire(_) => false,
            };
            unreadable |= !readable;
        }
        if unreadable {
            continue;
        }
        let mut ready = [None; payload::SMO_MAX_PAGES];
        let mut held_back = false;
        for (((item, change), page), to) in
            group.iter().zip(&changes).zip(pages.iter_mut()).zip(ready.iter_mut())
        {
            if !is_next(carried.get(&item.page).copied().unwrap_or(Base::Unwritten), item.prev) {
                continue;
            }
            let Ok(pid) = PageId::new(item.page) else { continue };
            if (change.is_op() && !pending.read(item.page, page) && store.read(pid, page).is_err())
                || change.rebuild(page).is_err()
            {
                held_back = true;
                break;
            }
            page[crate::node::offsets::GSN..crate::node::offsets::GSN + 8]
                .copy_from_slice(&item.gsn.to_le_bytes());
            crate::page::checksum::stamp(page, pid);
            *to = Some(pid);
        }
        if held_back {
            continue;
        }
        for ((item, page), pid) in group.iter().zip(&pages).zip(ready) {
            let Some(pid) = pid else { continue };
            pending.put(store, pid, page)?;
            wrote.insert(item.page);
            #[cfg(feature = "stats")]
            if item.op && torn_based.contains(&item.page) {
                TORN_REBUILT_WITH_CHANGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            carried.insert(item.page, Base::At(item.gsn));
            let image =
                <&[u8; crate::PAGE_SIZE]>::try_from(page.as_slice()).map_err(|_| Error::Corrupt)?;
            on_device_now.insert(item.page, AfterRedo::of(image));
            applied += 1;
        }
        effective.insert((worker, record));
    }
    pending.write_all(store)?;
    let mut page = vec![0u8; crate::PAGE_SIZE];
    for &skipped in read_whole.difference(&wrote) {
        let pid = PageId::new(skipped)?;
        store.read(pid, &mut page)?;
        store.write(pid, &page)?;
    }
    store.barrier()?;
    Ok((applied, on_device_now, effective))
}
#[derive(Debug, Default)]
struct Chain {
    records: Vec<(u64, u64)>,
    by_prev: Vec<(u64, u64)>,
}
fn chains(queue: &[Apply]) -> HashMap<u64, Chain> {
    let mut chains: HashMap<u64, Chain> = HashMap::new();
    for item in queue {
        chains.entry(item.page).or_default().records.push((item.gsn, item.prev));
    }
    for chain in chains.values_mut() {
        let mut by_prev: Vec<(u64, u64)> =
            chain.records.iter().map(|&(at, prev)| (prev, at)).collect();
        by_prev.sort_unstable();
        let mut highest = 0;
        for entry in &mut by_prev {
            highest = highest.max(entry.1);
            entry.1 = highest;
        }
        chain.by_prev = by_prev;
    }
    chains
}
fn images(queue: &[Apply]) -> HashMap<u64, Vec<(u64, u64)>> {
    let mut images: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();
    for item in queue.iter().filter(|a| !a.op) {
        images.entry(item.page).or_default().push((item.gsn, item.prev));
    }
    images
}
fn skipped(chain: Option<&Chain>, gsn: u64) -> bool {
    chain.is_some_and(|c| {
        let below = c.by_prev.partition_point(|&(prev, _)| prev < gsn);
        below > 0 && c.by_prev[below - 1].1 > gsn
    })
}
fn on_its_page(on_page: u64, gsn: u64, chain: Option<&Chain>) -> bool {
    on_page >= gsn && !skipped(chain, gsn)
}
fn group_applies(standing: &[(bool, bool)]) -> bool {
    standing.iter().all(|&(next, past)| next || past) && standing.iter().any(|&(next, _)| next)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Base {
    Unwritten,
    Torn,
    At(u64),
}
impl Base {
    fn stamp(self) -> Option<u64> {
        match self {
            Self::At(stamp) => Some(stamp),
            Self::Unwritten | Self::Torn => None,
        }
    }
}
fn is_next(base: Base, prev: u64) -> bool {
    match base {
        Base::Unwritten => prev == 0,
        Base::Torn => false,
        Base::At(on_page) => on_page == prev,
    }
}
fn torn_start(
    chain: Option<&Vec<(u64, u64)>>,
    images: Option<&Vec<(u64, u64)>>,
    floor: u64,
) -> Option<u64> {
    crate::calc::torn_start(chain?, images?, floor)
}
pub fn undo(analysis: &Analysis, tree: &crate::Tree) -> Result<usize, Error> {
    undo_counted(analysis, tree).map(|undone| undone.restored)
}
fn undo_refused(e: Error) -> Error {
    match e {
        Error::Device { .. } | Error::StoreFull { .. } => e,
        _ => Error::RecoveryFailed,
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Undone {
    pub restored: usize,
    pub held: usize,
    pub runs: Vec<(u64, u64)>,
}
pub fn undo_counted(analysis: &Analysis, tree: &crate::Tree) -> Result<Undone, Error> {
    let mut pending: Vec<(u64, u16, &Logged)> = Vec::new();
    for (worker, ring) in analysis.rings.iter().enumerate() {
        let worker = u16::try_from(worker).unwrap_or(u16::MAX);
        for logged in &ring.logged {
            if logged.kind != Kind::Change {
                continue;
            }
            if analysis.outcomes.get(&(worker, logged.segment, logged.txn))
                == Some(&Outcome::Committed)
            {
                continue;
            }
            pending.push((logged.gsn, worker, logged));
        }
    }
    pending.sort_by_key(|&(gsn, _, _)| std::cmp::Reverse(gsn));
    let rings = analysis.log.rings()?;
    let reads = Payloads::reads(rings, analysis.rings.len());
    let mut payloads = Payloads::new(rings, &reads, crate::PAGE_SIZE);
    let mut undone = Undone::default();
    for (_, worker, logged) in pending {
        let _bound = crate::wal::worker::bind(worker);
        let body = payloads.payload(usize::from(worker), logged)?;
        let Ok(entry) = payload::read_change_undo(body) else {
            return Err(Error::RecoveryFailed);
        };
        let head = entry.head;
        let restored = if let Some(before) = entry.before {
            tree.restore_stored(entry.key, head, before).map_err(undo_refused)?
        } else {
            let held = tree.get(entry.key).map_err(undo_refused)?;
            if held.is_some_and(|v| v.starts_with(head)) {
                tree.remove(entry.key).map_err(undo_refused)?;
                true
            } else {
                false
            }
        };
        if restored {
            undone.restored += 1;
            if let Some((reference, kept)) = entry.before.and_then(crate::node::blob::of_token) {
                let pages = crate::node::blob::pages_for(reference.rest(kept.len()));
                undone.runs.push((reference.first, pages));
            }
        } else {
            undone.held += 1;
        }
    }
    Ok(undone)
}
