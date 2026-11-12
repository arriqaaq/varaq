use crate::bufmgr::cooling;
use crate::bufmgr::fault::Route;
use crate::bufmgr::parent::Children;
use crate::bufmgr::pool::{PageLatch, Pool};
use crate::bufmgr::provider::{Budget, Counters, Sampler};
use crate::child::ChildRef;
use crate::latch::HybridLatch;
use crate::node::reader::NodeRef;
use crate::node::{Node, PAGE_SIZE, PageKind};
use crate::store::{MemoryStore, PageStore};
use crate::task::Scheduler;
use crate::wal::record::payload::Change;
use crate::{Error, Optimistic, PageId, Unwind};
use std::sync::Arc;
pub(crate) const RETRY_BOUND: u32 = 1_000_000;
pub(crate) const FALLBACK_AFTER: u32 = 2;
pub(crate) const YIELD_AFTER: u32 = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadMode {
    Optimistic,
    Held,
}
pub(crate) const MISROUTE_BOUND: u32 = 64;
pub const NODE_WORDS: usize = PAGE_SIZE / 8;
const ROOTLESS: ChildRef = ChildRef::evicted(match PageId::new(u64::MAX >> 2) {
    Ok(p) => p,
    Err(_) => unreachable!(),
});
pub const DEFAULT_CAPACITY: usize = 1 << 16;
const PROMOTE_ODDS: u64 = 32;
const DISPLACE_ODDS: u64 = 16;
#[derive(Debug)]
pub enum Stop {
    Retry,
    Misrouted,
    Refuse(Error),
}
impl From<Unwind> for Stop {
    fn from(_: Unwind) -> Self {
        Self::Retry
    }
}
impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Self::Refuse(e)
    }
}
pub type Descent<T> = Result<T, Stop>;
#[derive(Clone, Copy)]
pub(crate) struct Resolved<'t> {
    pub(crate) latch: &'t PageLatch,
    pub(crate) page: PageId,
    pub(crate) frame: u32,
    pub(crate) sample: crate::frame::Sample,
}
pub struct Tree {
    pool: Pool,
    store: std::sync::Arc<dyn PageStore>,
    evictions: crate::stats::Stat,
    forced_log_flushes: crate::stats::Stat,
    scan_hops: crate::stats::Stat,
    scan_chunks: crate::stats::Stat,
    scan_chunks_refused: crate::stats::Stat,
    scan_absent: crate::stats::Stat,
    scan_absent_after_skip: crate::stats::Stat,
    reads_held: crate::stats::Stat,
    leaves_reused: crate::stats::Stat,
    leaves_redescended: crate::stats::Stat,
    announcements: crate::stats::Stat,
    smo_restarts: crate::stats::Stat,
    yields: crate::stats::Stat,
    separate_above: usize,
    keep_local: usize,
    room: value::Room,
    blobs: blob::BlobRuns,
    data_barrier: Option<std::sync::Arc<blob::DataBarrier>>,
    value_write_refaults: crate::stats::Stat,
    cooling_warmed: crate::stats::Stat,
    values_placed: crate::stats::Stat,
    pages_imaged: crate::stats::Stat,
    pages_changed: crate::stats::Stat,
    value_reads: crate::stats::Stat,
    value_relocations: crate::stats::Stat,
    value_copies_unnamed: crate::stats::Stat,
    value_retries: [crate::stats::Stat; value::RETRY_SITES],
    merges: crate::stats::Stat,
    reissued: crate::stats::Stat,
    evictions_in_smo: crate::stats::Stat,
    in_smo: crate::stats::Stat,
    wal: Option<std::sync::Arc<crate::wal::worker::Wal>>,
    stops: Option<std::sync::Arc<crate::wal::worker::Wal>>,
    unanswered: core::sync::atomic::AtomicBool,
    root: HybridLatch<ChildRef, 1>,
    channels: Vec<crate::bufmgr::fault::WorkerChannel>,
    scheduler: Option<Arc<dyn Scheduler>>,
    translation: crate::bufmgr::Translation,
    inline_seed: core::sync::atomic::AtomicU64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Room {
    Made,
    Undecided,
    Nothing,
}
fn unnamed_is_a_retry(e: Error) -> Stop {
    match e {
        Error::FaultFailed { why: crate::bufmgr::fault::Why::Unnamed, .. } => Stop::Retry,
        e => Stop::Refuse(e),
    }
}
#[must_use = "a new page is held until its record stamps it; dropped early, the provider may evict it with no write"]
pub(crate) struct Fresh<'t> {
    child: ChildRef,
    frame: u32,
    latch: &'t PageLatch,
    held: Option<crate::latch::ExclusiveGuard<'t, Node, NODE_WORDS>>,
}
impl Fresh<'_> {
    pub(crate) fn child(&self) -> ChildRef {
        self.child
    }
    pub(crate) fn frame(&self) -> u32 {
        self.frame
    }
    pub(crate) fn image(&self) -> Result<Vec<u8>, Error> {
        match &self.held {
            Some(held) => Ok(Tree::image_of(held)),
            None => self
                .latch
                .exclusive()
                .map(|g| Tree::image_of(&g))
                .map_err(|_| Error::Livelocked { attempts: crate::latch::SPIN_BUDGET }),
        }
    }
    pub(crate) fn release(self) -> ChildRef {
        self.child
    }
}
#[derive(Clone)]
pub struct Leaf<'t> {
    pub child: ChildRef,
    pub frame: u32,
    pub latch: &'t PageLatch,
    pub guard: crate::OptimisticGuard<'t, Node, NODE_WORDS>,
    pub(crate) sample: crate::frame::Sample,
}
impl core::fmt::Debug for Leaf<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Leaf")
            .field("child", &self.child)
            .field("frame", &self.frame)
            .field("version", &self.guard.sampled())
            .finish_non_exhaustive()
    }
}
pub(crate) struct Level<'t> {
    pub(crate) child: ChildRef,
    pub(crate) resolved: Resolved<'t>,
    pub(crate) guard: crate::OptimisticGuard<'t, Node, NODE_WORDS>,
}
pub(crate) trait Trail<'t> {
    fn pass(&mut self, level: Level<'t>);
    #[inline(always)]
    fn refused<T: crate::latch::AnyBits, const W: usize>(&mut self, _latch: &'t HybridLatch<T, W>) {
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hold {
    latch: usize,
    version: u64,
}
thread_local! {
    static REFUSED_BY : core::cell::Cell < Option < Hold >> = const {
    core::cell::Cell::new(None) };
}
#[derive(Debug, Default)]
pub(crate) struct Refusals {
    counted: u32,
    run: u32,
    last: Option<Hold>,
}
impl Refusals {
    pub(crate) fn retry(&mut self) -> bool {
        let hold = REFUSED_BY.take();
        self.run = self.run.saturating_add(1);
        if hold.is_none() || hold != self.last {
            self.counted += 1;
        }
        self.last = hold;
        self.counted >= RETRY_BOUND
    }
    pub(crate) fn other(&mut self) -> bool {
        REFUSED_BY.set(None);
        self.run = self.run.saturating_add(1);
        self.counted += 1;
        self.last = None;
        self.counted >= RETRY_BOUND
    }
}
#[derive(Default)]
pub(crate) struct Announced<'t>(Vec<crate::latch::Waiting<'t>>);
impl<'t> Announced<'t> {
    pub(crate) fn on<T: crate::latch::AnyBits, const W: usize>(
        &mut self,
        latch: &'t HybridLatch<T, W>,
    ) {
        if self.0.iter().any(|w| latch.is_announced_by(w)) {
            return;
        }
        if let Some(waiting) = latch.announce() {
            self.0.push(waiting);
        }
    }
}
impl<'t> Trail<'t> for Announced<'t> {
    #[inline(always)]
    fn pass(&mut self, _level: Level<'t>) {}
    fn refused<T: crate::latch::AnyBits, const W: usize>(&mut self, latch: &'t HybridLatch<T, W>) {
        self.on(latch);
    }
}
impl<'t> Trail<'t> for () {
    #[inline(always)]
    fn pass(&mut self, _level: Level<'t>) {}
}
impl<'t> Trail<'t> for Vec<Level<'t>> {
    fn pass(&mut self, level: Level<'t>) {
        self.push(level);
    }
}
thread_local! {
    static LOOKUP_BUFFER : core::cell::Cell < Vec < u8 >> = const {
    core::cell::Cell::new(Vec::new()) };
}
impl Tree {
    pub fn new() -> Result<Self, Error> {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
    pub fn with_capacity(capacity: usize) -> Result<Self, Error> {
        Self::with_store(capacity, Self::default_store(capacity))
    }
    pub(crate) fn default_store(capacity: usize) -> Box<dyn PageStore> {
        Box::new(MemoryStore::new(capacity.saturating_mul(16)))
    }
    pub fn with_cooling_percent(capacity: usize, percent: usize) -> Result<Self, Error> {
        Self::with_store_and_cooling(
            capacity,
            Box::new(MemoryStore::new(capacity.saturating_mul(16))),
            percent,
        )
    }
    pub fn with_store(capacity: usize, store: Box<dyn PageStore>) -> Result<Self, Error> {
        if capacity == 0 {
            return Err(Error::Full { capacity });
        }
        Self::with_store_and_cooling(capacity, store, cooling::DEFAULT_COOLING_PERCENT)
    }
    pub fn with_store_and_cooling(
        capacity: usize,
        store: Box<dyn PageStore>,
        percent: usize,
    ) -> Result<Self, Error> {
        Self::rooted(Self::empty(capacity, store, percent)?)
    }
    pub(crate) fn with_store_reissuing(
        capacity: usize,
        store: Box<dyn PageStore>,
        reissuable: crate::bufmgr::pool::Reissuable,
    ) -> Result<Self, Error> {
        if capacity == 0 {
            return Err(Error::Full { capacity });
        }
        let mut tree = Self::empty(capacity, store, cooling::DEFAULT_COOLING_PERCENT)?;
        tree.pool.share_reissuable(reissuable);
        Self::rooted(tree)
    }
    fn rooted(tree: Self) -> Result<Self, Error> {
        let root =
            tree.allocate_maybe_logged(&Node::new(PageKind::Leaf, b"", b""), false)?.release();
        tree.root
            .exclusive()
            .map_err(|_| Error::Livelocked { attempts: crate::latch::SPIN_BUDGET })?
            .set(root);
        Ok(tree)
    }
    pub fn with_store_logged(
        capacity: usize,
        store: Box<dyn PageStore>,
        wal: std::sync::Arc<crate::wal::worker::Wal>,
    ) -> Result<Self, Error> {
        let mut tree = Self::empty(capacity, store, cooling::DEFAULT_COOLING_PERCENT)?;
        tree.data_barrier = wal.data_barrier_for(&tree.store);
        tree.wal = Some(wal);
        let root = tree.allocate_maybe_logged(&Node::new(PageKind::Leaf, b"", b""), true)?;
        tree.root
            .exclusive()
            .map_err(|_| Error::Livelocked { attempts: crate::latch::SPIN_BUDGET })?
            .set(root.child());
        tree.mark_frame_dirty(root.frame(), &root.image()?);
        drop(root);
        tree.refuse_if_poisoned()?;
        Ok(tree)
    }
    fn empty(capacity: usize, store: Box<dyn PageStore>, percent: usize) -> Result<Self, Error> {
        if capacity == 0 {
            return Err(Error::Full { capacity });
        }
        let partitions = (capacity / 64).clamp(1, 64);
        let pool = Pool::with_cooling_percent(capacity, partitions, percent)?;
        let tree = Self {
            wal: None,
            stops: None,
            unanswered: core::sync::atomic::AtomicBool::new(false),
            channels: Vec::new(),
            scheduler: None,
            pool,
            store: std::sync::Arc::from(store),
            evictions: crate::stats::Stat::default(),
            forced_log_flushes: crate::stats::Stat::default(),
            scan_hops: crate::stats::Stat::default(),
            scan_chunks: crate::stats::Stat::default(),
            reads_held: crate::stats::Stat::default(),
            leaves_reused: crate::stats::Stat::default(),
            leaves_redescended: crate::stats::Stat::default(),
            announcements: crate::stats::Stat::default(),
            scan_chunks_refused: crate::stats::Stat::default(),
            scan_absent: crate::stats::Stat::default(),
            scan_absent_after_skip: crate::stats::Stat::default(),
            smo_restarts: crate::stats::Stat::default(),
            yields: crate::stats::Stat::default(),
            separate_above: crate::node::SEPARATE_ABOVE,
            keep_local: 0,
            room: value::Room::default(),
            blobs: blob::BlobRuns::default(),
            data_barrier: None,
            value_write_refaults: crate::stats::Stat::default(),
            cooling_warmed: crate::stats::Stat::default(),
            values_placed: crate::stats::Stat::default(),
            pages_imaged: crate::stats::Stat::default(),
            pages_changed: crate::stats::Stat::default(),
            value_reads: crate::stats::Stat::default(),
            value_relocations: crate::stats::Stat::default(),
            value_copies_unnamed: crate::stats::Stat::default(),
            value_retries: Default::default(),
            merges: crate::stats::Stat::default(),
            reissued: crate::stats::Stat::default(),
            evictions_in_smo: crate::stats::Stat::default(),
            in_smo: crate::stats::Stat::default(),
            inline_seed: core::sync::atomic::AtomicU64::new(0x9E37_79B9_7F4A_7C15),
            root: HybridLatch::new(ROOTLESS),
            translation: crate::bufmgr::Translation::new(capacity),
        };
        Ok(tree)
    }
    pub fn open(capacity: usize, store: Box<dyn PageStore>, root: u64) -> Result<Self, Error> {
        let root = PageId::new(root).map_err(|_| Error::Corrupt)?;
        let tree = Self::empty(capacity, store, cooling::DEFAULT_COOLING_PERCENT)?;
        tree.root
            .exclusive()
            .map_err(|_| Error::Livelocked { attempts: crate::latch::SPIN_BUDGET })?
            .set(ChildRef::evicted(root));
        Ok(tree)
    }
    fn allocate(&self, node: &Node) -> Result<Fresh<'_>, Error> {
        self.allocate_maybe_logged(node, self.wal.is_some())
    }
    fn allocate_maybe_logged(&self, node: &Node, logged: bool) -> Result<Fresh<'_>, Error> {
        let (page, gsn) = match self.take_reissuable() {
            Some(reissued) => reissued,
            None => (self.store.allocate()?, 0),
        };
        self.allocate_as(node, logged, page, gsn)
    }
    fn release_unnamed(&self, index: u32, page: PageId, gsn: u64) {
        crate::bufmgr::fault::release_frame(&self.pool, index);
        if self.pool.slot(index).is_none_or(|s| s.frame.page_id() != Some(page)) {
            self.give_back(page, gsn);
        }
    }
    fn take_reissuable(&self) -> Option<(PageId, u64)> {
        let reissued = self.pool.take_reissuable();
        if let Some((page, _)) = reissued {
            self.reissued.add(1);
            debug_assert!(
                !self.pool.owed_pages().contains(&page),
                "page {page:?} is handed out again while the device is owed its retired image"
            );
        }
        reissued
    }
    fn give_back(&self, page: PageId, gsn: u64) {
        debug_assert!(
            !matches!(self.translation.lookup(page), Ok(Some(_))),
            "page {page:?} is given back while the translation layer still names it"
        );
        self.pool.give_back(page, gsn);
    }
    fn allocate_as(
        &self,
        node: &Node,
        logged: bool,
        page: PageId,
        gsn: u64,
    ) -> Result<Fresh<'_>, Error> {
        let index = self
            .with_room(|| self.pool.acquire(page).ok_or(Error::Full { capacity: self.pool.len() }))
            .inspect_err(|_| self.give_back(page, gsn))?;
        let Some(slot) = self.pool.slot(index) else {
            crate::bufmgr::fault::release_frame(&self.pool, index);
            self.give_back(page, gsn);
            return Err(Error::Corrupt);
        };
        let Some(mut guard) = crate::bufmgr::fault::latch_claimed(&slot.page) else {
            self.release_unnamed(index, page, gsn);
            return Err(Error::Livelocked { attempts: RETRY_BOUND });
        };
        guard.set(*node);
        if slot.frame.arrive(if logged { gsn } else { 0 }).is_err() {
            drop(guard);
            self.release_unnamed(index, page, gsn);
            return Err(Error::Corrupt);
        }
        if !logged {
            slot.frame.modified(1);
        }
        self.translation
            .insert(page, index)
            .map_err(|_| Error::Livelocked { attempts: crate::latch::SPIN_BUDGET })?;
        Ok(Fresh {
            child: ChildRef::evicted(page),
            frame: index,
            latch: &slot.page,
            held: Some(guard),
        })
    }
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.pool.len()
    }
    #[must_use]
    pub fn nodes_used(&self) -> usize {
        (0..u32::try_from(self.pool.len()).unwrap_or(u32::MAX))
            .filter(|&i| {
                self.pool.slot(i).is_some_and(|s| s.frame.state() != crate::frame::State::Free)
            })
            .count()
    }
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }
    #[must_use]
    pub fn bucket_addresses(&self) -> core::ops::Range<usize> {
        self.translation.bucket_addresses()
    }
    pub fn root_page(&self) -> Result<Option<PageId>, Error> {
        self.retrying(|| {
            let root = self.root.optimistic().inspect_err(|_| {
                Self::refused_by(&self.root);
            })?;
            Ok(root.read()?.page_id())
        })
    }
    #[must_use]
    pub fn store(&self) -> &dyn PageStore {
        &*self.store
    }
    #[must_use]
    pub fn bytes_resident(&self) -> usize {
        self.nodes_used() * PAGE_SIZE
    }
    #[inline]
    fn frame_index(&self, child: ChildRef) -> Option<u32> {
        let page = child.page_id()?;
        for _ in 0..RETRY_BOUND {
            match self.translation.lookup(page) {
                Ok(frame) => return frame,
                Err(Unwind) => crate::latch::spin(),
            }
        }
        None
    }
    fn resolve(&self, child: ChildRef) -> Optimistic<Resolved<'_>> {
        let page = child.page_id().ok_or(Unwind)?;
        let (latch, frame, sample) = self.predict(page)?.ok_or(Unwind)?;
        Ok(Resolved { latch, page, frame, sample })
    }
    fn confirm(&self, resolved: &Resolved<'_>) -> Optimistic<()> {
        let slot = self.pool.slot(resolved.frame).ok_or(Unwind)?;
        if slot.frame.page_id() == Some(resolved.page) && slot.frame.recheck(resolved.sample) {
            Ok(())
        } else {
            Err(Unwind)
        }
    }
    fn predict(&self, page: PageId) -> Optimistic<Option<(&PageLatch, u32, crate::frame::Sample)>> {
        let hash = crate::bufmgr::translate::mix(page.get());
        #[allow(clippy::cast_possible_truncation)]
        let predicted = crate::bufmgr::translate::preferred_frame_of(hash, self.pool.len() as u32);
        let guess = self.pool.slot(predicted).ok_or(Unwind)?;
        let sampled = guess.frame.sample();
        let Some(found) = self.lookup_noted(page, hash)? else {
            return Ok(None);
        };
        if found == predicted
            && sampled.state().is_readable()
            && guess.frame.page_id() == Some(page)
            && guess.frame.recheck(sampled)
        {
            self.warm_if_cooling(&guess.frame, sampled)?;
            return Ok(Some((&guess.page, predicted, sampled)));
        }
        let slot = self.pool.slot(found).ok_or(Unwind)?;
        let seen = slot.frame.sample();
        if !seen.state().is_readable()
            || slot.frame.page_id() != Some(page)
            || !slot.frame.recheck(seen)
        {
            return Err(Unwind);
        }
        if self.lookup_noted(page, hash)? != Some(found) {
            return Err(Unwind);
        }
        self.warm_if_cooling(&slot.frame, seen)?;
        Ok(Some((&slot.page, found, seen)))
    }
    fn warm_if_cooling(
        &self,
        frame: &crate::frame::BufferFrame,
        sample: crate::frame::Sample,
    ) -> Optimistic<()> {
        if sample.state() != crate::frame::State::Cool {
            return Ok(());
        }
        if frame.warm().is_ok() {
            self.cooling_warmed.add(1);
        }
        Err(Unwind)
    }
    fn fault_predictive(&self, page: PageId, route: Route) -> Descent<()> {
        let _named = self.with_room(|| self.read_page(page, route)).map_err(unnamed_is_a_retry)?;
        Ok(())
    }
    fn name_loaded(&self, page: PageId, index: u32, route: Route) -> Result<u32, Error> {
        let slot = self.pool.slot(index).ok_or(Error::Corrupt)?;
        let holds = |f: u32| {
            self.pool
                .slot(f)
                .is_some_and(|s| s.frame.state().is_readable() && s.frame.page_id() == Some(page))
        };
        debug_assert!(
            route != Route::Held
                || slot.page.optimistic().ok().and_then(|g| NodeRef::new(&g).kind().ok())
                    != Some(PageKind::Value as u8),
            "a value page named under Route::Held"
        );
        let still = || {
            self.routed(route)
                && slot.frame.state().is_readable()
                && slot.frame.page_id() == Some(page)
        };
        match self.translation.insert_if(page, index, still, holds).map_err(|_| Error::Corrupt)? {
            crate::bufmgr::translate::Recorded::Yes => Ok(index),
            crate::bufmgr::translate::Recorded::Gone => {
                Err(Error::FaultFailed { page, why: crate::bufmgr::fault::Why::Unnamed })
            }
            crate::bufmgr::translate::Recorded::Elsewhere(named) => {
                crate::bufmgr::fault::unnamed_back(&self.pool, page, index);
                Ok(named)
            }
        }
    }
    fn routed(&self, route: Route) -> bool {
        match route {
            Route::Held => true,
            Route::Parent { frame, version } => {
                self.pool.slot(frame).is_some_and(|s| s.page.unchanged_since(version))
            }
            Route::Root { version } => self.root.unchanged_since(version),
        }
    }
    fn maybe_promote(&self, page: PageId, from: u32) {
        #[allow(clippy::cast_possible_truncation)]
        let frames = self.pool.len() as u32;
        let preferred = crate::bufmgr::preferred_frame(page, frames);
        if preferred == from {
            return;
        }
        let Some(target) = self.pool.slot(preferred) else { return };
        let roll = self
            .inline_seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed);
        if !crate::bufmgr::translate::mix(roll).is_multiple_of(PROMOTE_ODDS) {
            return;
        }
        let occupied = target.frame.state() != crate::frame::State::Free;
        if occupied
            && let Some(resident) = target.frame.page_id()
            && crate::bufmgr::preferred_frame(resident, frames) == preferred
        {
            let again = self
                .inline_seed
                .fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed);
            if !crate::bufmgr::translate::mix(again).is_multiple_of(DISPLACE_ODDS) {
                return;
            }
        }
        self.promote_into(page, from, preferred, occupied);
    }
    fn promote_into(&self, page: PageId, from: u32, preferred: u32, occupied: bool) {
        let Some(target) = self.pool.slot(preferred) else { return };
        let Some(source) = self.pool.slot(from) else { return };
        let Ok(hot) = source.page.exclusive() else { return };
        if source.frame.page_id() != Some(page) {
            return;
        }
        if source.frame.state() != crate::frame::State::Hot
            || source.frame.writebacks_in_flight() > 0
        {
            return;
        }
        if !matches!(self.translation.lookup(page), Ok(Some(named)) if named == from) {
            return;
        }
        if occupied && !self.demote(preferred) {
            return;
        }
        if !self.pool.claim(preferred, page) {
            return;
        }
        let node = hot.get();
        let Ok(mut moved) = target.page.exclusive() else {
            if target.frame.abandon_load().is_ok() {
                let _ = self.pool.release(preferred);
            }
            return;
        };
        moved.set(node);
        if target.frame.take_over(&source.frame, || {}).is_err() {
            drop(moved);
            if target.frame.abandon_load().is_ok() {
                let _ = self.pool.release(preferred);
            }
            return;
        }
        if self.translation.insert(page, preferred).is_err() {
            return;
        }
        self.pool.note_move(from, preferred);
        drop(moved);
        if source.frame.reclaim().is_ok() {
            let _ = self.pool.release(from);
        }
        drop(hot);
    }
    fn demote(&self, frame: u32) -> bool {
        let Some(slot) = self.pool.slot(frame) else { return false };
        let Some(victim) = slot.frame.page_id() else { return false };
        if slot.frame.state() != crate::frame::State::Hot {
            return false;
        }
        let Ok(held) = slot.page.exclusive() else { return false };
        if slot.frame.page_id() != Some(victim)
            || slot.frame.state() != crate::frame::State::Hot
            || slot.frame.writebacks_in_flight() > 0
        {
            return false;
        }
        if !matches!(
            self.translation.lookup(victim), Ok(Some(named)) if named == frame
        ) {
            return false;
        }
        let Some(fresh) = self.pool.acquire(victim) else { return false };
        let Some(home) = self.pool.slot(fresh) else { return false };
        let Ok(mut clone) = home.page.exclusive() else {
            if home.frame.abandon_load().is_ok() {
                let _ = self.pool.release(fresh);
            }
            return false;
        };
        clone.set(held.get());
        let arrived = home.frame.take_over(&slot.frame, || {});
        if arrived.is_err() {
            drop(clone);
            if home.frame.abandon_load().is_ok() {
                let _ = self.pool.release(fresh);
            }
            return false;
        }
        if self.translation.insert(victim, fresh).is_err() {
            return false;
        }
        self.pool.note_move(frame, fresh);
        drop(clone);
        let freed = match slot.frame.reclaim() {
            Ok(()) => {
                let _ = self.pool.release(frame);
                true
            }
            Err(_) => false,
        };
        drop(held);
        freed
    }
    pub fn lookup<R>(&self, key: &[u8], f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>, Error> {
        let mut big = LOOKUP_BUFFER.with(core::cell::Cell::take);
        let mut probe = value::Probe::lending(&mut big);
        let found = self.read_into(key, &mut probe);
        let out = found.map(|len| len.map(|len| f(probe.bytes(len))));
        if big.capacity() <= blob::KEPT_MOST {
            LOOKUP_BUFFER.with(|cell| cell.set(big));
        }
        out
    }
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let mut out = Vec::new();
        Ok(self.read_into(key, &mut out)?.map(|_| out))
    }
    fn read_entry<'t>(
        &'t self,
        key: &[u8],
        (mode, stored): (ReadMode, bool),
        out: &mut (impl value::ValueSink + ?Sized),
        (announced, seen): (&mut Announced<'t>, Option<&mut Option<Leaf<'t>>>),
    ) -> Descent<Option<usize>> {
        let reached = match mode {
            ReadMode::Optimistic => self.descend(key)?,
            ReadMode::Held => self.descend_trailed(key, announced)?,
        };
        if let Some(seen) = seen {
            *seen = Some(reached.clone());
        }
        let Leaf { frame: leaf_frame, latch: leaf_latch, guard: leaf_guard, .. } = reached;
        if mode == ReadMode::Held {
            let sample = leaf_guard.clone();
            let held = leaf_guard.try_shared().inspect_err(|_| {
                announced.on(leaf_latch);
                Self::refused_by(leaf_latch);
            })?;
            let leaf = NodeRef::shared(&held);
            let (at, hit) = leaf.lower_bound(key)?;
            if !hit {
                return Ok(None);
            }
            if stored && let Some(len) = Self::token_into(&leaf, (at, leaf.slot(at)?.4), out)? {
                return Ok(Some(len));
            }
            if leaf.slot(at)?.4 & crate::node::flags::BLOB != 0 {
                let (reference, kept) =
                    value::Blob::read(&leaf, at)?.ok_or(Stop::Refuse(Error::Corrupt))?;
                drop(held);
                return self.blob_read(key, reference, kept.bytes(), out).map(Some);
            }
            return match self.entry_value_held(&leaf, at, key, out, announced)? {
                value::Value::Read(len) => Ok(Some(len)),
                value::Value::Absent(page) => {
                    drop(held);
                    self.retried(value::RetrySite::ReadFault);
                    self.fault_value(page, leaf_frame, &sample)?;
                    Err(Stop::Retry)
                }
            };
        }
        let leaf = NodeRef::new(&leaf_guard);
        let (at, hit) = leaf.lower_bound(key)?;
        if !hit {
            leaf_guard.recheck()?;
            return Ok(None);
        }
        let flags = leaf.slot(at)?.4;
        if !stored && flags & crate::node::flags::BLOB != 0 {
            let Some((reference, kept)) = value::Blob::read(&leaf, at)? else {
                return Err(Stop::Retry);
            };
            leaf_guard.recheck()?;
            let len = self.blob_read(key, reference, kept.bytes(), out)?;
            return Ok(Some(len));
        }
        let len = match stored
            .then(|| Self::token_into(&leaf, (at, flags), out))
            .transpose()?
            .flatten()
        {
            Some(len) => len,
            None => self.entry_value_into(&leaf, &leaf_guard, leaf_frame, (at, flags), key, out)?,
        };
        leaf_guard.recheck()?;
        Ok(Some(len))
    }
    pub(crate) fn read_into(
        &self,
        key: &[u8],
        out: &mut (impl value::ValueSink + ?Sized),
    ) -> Result<Option<usize>, Error> {
        self.refuse_if_poisoned()?;
        self.retrying_read(|mode, announced| {
            match self.read_entry(key, (mode, false), out, (announced, None))? {
                Some(len) if !out.holds(len) => Err(Stop::Retry),
                found => Ok(found),
            }
        })
    }
    pub(crate) fn read_stored_into<'t>(
        &'t self,
        key: &[u8],
        out: &mut (impl value::ValueSink + ?Sized),
        seen: &mut Option<Leaf<'t>>,
    ) -> Result<Option<usize>, Error> {
        self.refuse_if_poisoned()?;
        self.retrying_read(|mode, announced| {
            match self.read_entry(key, (mode, true), out, (announced, Some(&mut *seen)))? {
                Some(len) if !out.holds(len) => Err(Stop::Retry),
                found => Ok(found),
            }
        })
    }
    fn token_into<const W: usize>(
        leaf: &NodeRef<'_, '_, W>,
        (at, flags): (usize, u16),
        out: &mut (impl value::ValueSink + ?Sized),
    ) -> Descent<Option<usize>> {
        if flags & crate::node::flags::BLOB == 0 {
            return Ok(None);
        }
        let (reference, kept) = value::Blob::read(leaf, at)?.ok_or(Stop::Retry)?;
        let (token, len) = crate::node::blob::token(reference, kept.bytes());
        let room = out.sized(len);
        let n = room.len().min(len);
        room[..n].copy_from_slice(&token[..n]);
        Ok(Some(len))
    }
    fn retrying_read<'t, T>(
        &'t self,
        mut attempt: impl FnMut(ReadMode, &mut Announced<'t>) -> Descent<T>,
    ) -> Result<T, Error> {
        let mut refused = 0_u32;
        let mut announced = Announced::default();
        let outcome = self.retrying(|| {
            let mode =
                if refused >= FALLBACK_AFTER { ReadMode::Held } else { ReadMode::Optimistic };
            if mode == ReadMode::Held {
                self.reads_held.add(1);
            }
            let outcome = attempt(mode, &mut announced);
            if matches!(outcome, Err(Stop::Retry)) {
                refused = refused.saturating_add(1);
            }
            outcome
        });
        if !announced.0.is_empty() {
            self.announcements.add(announced.0.len() as u64);
        }
        outcome
    }
    fn retrying<T>(&self, mut attempt: impl FnMut() -> Descent<T>) -> Result<T, Error> {
        let mut misrouted = 0u32;
        let mut refusals = Refusals::default();
        loop {
            match attempt() {
                Ok(v) => return Ok(v),
                Err(Stop::Retry) => {
                    if refusals.retry() {
                        return Err(Error::Livelocked { attempts: RETRY_BOUND });
                    }
                    self.between_attempts(&refusals)?;
                }
                Err(Stop::Misrouted) => {
                    misrouted += 1;
                    let spent = refusals.other();
                    if misrouted >= MISROUTE_BOUND {
                        return Err(Error::Livelocked { attempts: misrouted });
                    }
                    if spent {
                        return Err(Error::Livelocked { attempts: RETRY_BOUND });
                    }
                    self.between_attempts(&refusals)?;
                }
                Err(Stop::Refuse(e)) => return Err(e),
            }
        }
    }
    fn lookup_noted(&self, page: PageId, hash: u64) -> Optimistic<Option<u32>> {
        self.translation
            .lookup_hashed(page, hash)
            .inspect_err(|_| Self::refused_by(self.translation.bucket_latch(hash)))
    }
    pub(crate) fn refused_by<T: crate::latch::AnyBits, const W: usize>(latch: &HybridLatch<T, W>) {
        let hold = (latch.is_held() && !latch.is_abandoned())
            .then(|| Hold { latch: core::ptr::from_ref(latch) as usize, version: latch.version() });
        REFUSED_BY.set(hold);
    }
    pub(crate) fn refused_by_writeback(&self, frame: u32) {
        let hold = self.pool.slot(frame).and_then(|s| {
            let version = s.frame.writeback_in_flight()?;
            Some(Hold { latch: core::ptr::from_ref(&s.frame) as usize, version })
        });
        REFUSED_BY.set(hold);
    }
    fn between_attempts(&self, refusals: &Refusals) -> Result<(), Error> {
        self.refuse_if_poisoned()?;
        self.yield_or_spin(refusals.run);
        Ok(())
    }
    fn yield_or_spin(&self, refused: u32) {
        match &self.scheduler {
            Some(scheduler) => scheduler.yield_now(),
            None if refused > YIELD_AFTER => {
                self.yields.add(1);
                crate::latch::give_back();
            }
            None => crate::latch::spin(),
        }
    }
    fn read_page(&self, page: PageId, route: Route) -> Result<u32, Error> {
        let io = crate::wal::worker::current().and_then(|w| self.channels.get(usize::from(w)));
        match io {
            Some(io) => crate::bufmgr::fault::fault_via(
                &self.pool,
                io,
                page,
                route,
                &|p| self.frame_index(ChildRef::evicted(p)),
                &|p, f, r| self.name_loaded(p, f, r),
            ),
            None => crate::bufmgr::fault::fault_named(
                &self.pool,
                &*self.store,
                page,
                route,
                &|p| self.frame_index(ChildRef::evicted(p)),
                &|p, f, r| self.name_loaded(p, f, r),
            ),
        }
    }
    pub(crate) fn read_span(
        &self,
        first: PageId,
        count: usize,
        route: Route,
    ) -> Result<usize, Error> {
        crate::bufmgr::fault::fault_span(
            &self.pool,
            &*self.store,
            first,
            count,
            route,
            &|p| self.frame_index(ChildRef::evicted(p)),
            &|p, f, r| self.name_loaded(p, f, r),
        )
    }
    #[must_use]
    pub fn with_wal(mut self, wal: std::sync::Arc<crate::wal::worker::Wal>) -> Self {
        self.data_barrier = wal.data_barrier_for(&self.store);
        self.wal = Some(wal);
        self
    }
    fn store_stop(&self) -> Option<&std::sync::Arc<crate::wal::worker::Wal>> {
        self.wal.as_ref().or(self.stops.as_ref())
    }
    pub(crate) fn stop_with(&mut self, wal: &std::sync::Arc<crate::wal::worker::Wal>) {
        self.stops = Some(std::sync::Arc::clone(wal));
    }
    #[must_use]
    pub fn with_separate_above(mut self, line: usize) -> Self {
        self.separate_above = line;
        self
    }
    #[must_use]
    pub fn value_retries(&self) -> [u64; value::RETRY_SITES] {
        core::array::from_fn(|i| self.value_retries[i].get())
    }
    pub fn set_separate_above(&mut self, line: usize) {
        self.separate_above = line;
    }
    #[must_use]
    pub fn separation_line(&self) -> usize {
        self.separate_above
    }
    pub fn set_keep_local(&mut self, n: usize) {
        self.keep_local = n.min(crate::node::separated::LOCAL_MOST);
    }
    #[must_use]
    pub fn keep_local(&self) -> usize {
        self.keep_local
    }
    #[must_use]
    pub fn values_placed(&self) -> u64 {
        self.values_placed.get()
    }
    #[must_use]
    pub fn page_records(&self) -> (u64, u64) {
        (self.pages_imaged.get(), self.pages_changed.get())
    }
    fn count_part(&self, change: &Change<'_>) {
        if change.is_op() { self.pages_changed.add(1) } else { self.pages_imaged.add(1) }
    }
    #[must_use]
    pub fn value_write_refaults(&self) -> u64 {
        self.value_write_refaults.get()
    }
    #[must_use]
    pub fn cooling_warmed(&self) -> u64 {
        self.cooling_warmed.get()
    }
    #[must_use]
    pub fn value_reads(&self) -> u64 {
        self.value_reads.get()
    }
    pub(crate) fn reissue_absorbed(
        &self,
        candidates: &[(u64, u64)],
        root: u64,
        redone: Option<&std::collections::HashMap<u64, crate::recovery::AfterRedo>>,
    ) {
        let mut newest: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
        for &(page, gsn) in candidates {
            let at = newest.entry(page).or_insert(gsn);
            *at = (*at).max(gsn);
        }
        let mut bytes = vec![0_u8; crate::PAGE_SIZE];
        for (page, gsn) in newest {
            let Ok(id) = PageId::new(page) else { continue };
            if page == root {
                continue;
            }
            let after = if let Some(&after) = redone.and_then(|record| record.get(&page)) {
                after
            } else {
                if self.store.read(id, &mut bytes).is_err() {
                    continue;
                }
                let Ok(image) = <&[u8; crate::PAGE_SIZE]>::try_from(bytes.as_slice()) else {
                    continue;
                };
                crate::recovery::AfterRedo::of(image)
            };
            if after.absorbed && after.stamp == gsn {
                self.give_back(id, gsn);
            }
        }
    }
    #[must_use]
    pub fn reissuable(&self) -> Vec<(u64, u64)> {
        self.pool.reissuable_snapshot()
    }
    #[must_use]
    pub fn owed_pages(&self) -> Vec<PageId> {
        self.pool.owed_pages()
    }
    #[must_use]
    pub fn ids_reissued(&self) -> u64 {
        self.reissued.get()
    }
    #[must_use]
    pub fn value_relocations(&self) -> u64 {
        self.value_relocations.get()
    }
    #[must_use]
    pub fn value_copies_unnamed(&self) -> u64 {
        self.value_copies_unnamed.get()
    }
    pub fn page_stamp(&self, key: &[u8]) -> Result<(u64, u32), Error> {
        self.retrying(|| {
            let leaf = self.descend(key)?;
            let slot = self.pool.slot(leaf.frame).ok_or(Unwind)?;
            let stamp = (slot.frame.gsn(), slot.frame.last_writer());
            leaf.guard.recheck()?;
            Ok(stamp)
        })
    }
    #[must_use]
    pub fn wal(&self) -> Option<&std::sync::Arc<crate::wal::worker::Wal>> {
        self.wal.as_ref()
    }
    #[must_use]
    pub fn with_channels(
        mut self,
        channels: Vec<Box<dyn crate::store::channel::IoChannel>>,
    ) -> Self {
        self.set_channels(channels);
        self
    }
    pub(crate) fn set_channels(
        &mut self,
        channels: Vec<Box<dyn crate::store::channel::IoChannel>>,
    ) {
        self.channels =
            channels.into_iter().map(crate::bufmgr::fault::WorkerChannel::new).collect();
    }
    #[must_use]
    pub fn with_channel(self, channel: Box<dyn crate::store::channel::IoChannel>) -> Self {
        self.with_channels(vec![channel])
    }
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Arc<dyn Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }
    pub fn descend(&self, key: &[u8]) -> Descent<Leaf<'_>> {
        self.descend_trailed(key, &mut ())
    }
    pub(crate) fn descend_from<'t>(
        &'t self,
        key: &[u8],
        from: Option<&Leaf<'t>>,
    ) -> Descent<Leaf<'t>> {
        if let Some(leaf) = from {
            if self.still_the_leaf(key, leaf) {
                self.leaves_reused.add(1);
                return Ok(leaf.clone());
            }
            self.leaves_redescended.add(1);
        }
        self.descend(key)
    }
    fn still_the_leaf(&self, key: &[u8], leaf: &Leaf<'_>) -> bool {
        let Some(page) = leaf.child.page_id() else { return false };
        let resolved = Resolved { latch: leaf.latch, page, frame: leaf.frame, sample: leaf.sample };
        self.confirm(&resolved).is_ok()
            && NodeRef::new(&leaf.guard).covers(key).unwrap_or(false)
            && leaf.guard.recheck().is_ok()
    }
    pub(crate) fn descend_trailed<'t>(
        &'t self,
        key: &[u8],
        trail: &mut impl Trail<'t>,
    ) -> Descent<Leaf<'t>> {
        let root_guard = self.root.optimistic().inspect_err(|_| {
            trail.refused(&self.root);
            Self::refused_by(&self.root);
        })?;
        let mut child = root_guard.read()?;
        root_guard.recheck()?;
        let mut above: Option<(crate::OptimisticGuard<'_, Node, NODE_WORDS>, u32)> = None;
        let mut via = 0_usize;
        let still_routed =
            |above: &Option<(crate::OptimisticGuard<'_, Node, NODE_WORDS>, u32)>| match above {
                Some((parent, _)) => parent.recheck(),
                None => root_guard.recheck(),
            };
        loop {
            let resolved = {
                let page = child.page_id().ok_or(Unwind)?;
                let Some((latch, frame, sample)) = self.predict(page)? else {
                    still_routed(&above)?;
                    let route = match &above {
                        Some((parent, frame)) => {
                            Route::Parent { frame: *frame, version: parent.sampled() }
                        }
                        None => Route::Root { version: root_guard.sampled() },
                    };
                    self.fault_predictive(page, route)?;
                    continue;
                };
                Resolved { latch, page, frame, sample }
            };
            let (latch, here, here_frame) = (resolved.latch, resolved.page, resolved.frame);
            let guard = latch.optimistic().inspect_err(|_| {
                trail.refused(latch);
                Self::refused_by(latch);
            })?;
            self.confirm(&resolved)?;
            still_routed(&above)?;
            if let Some((_, parent_frame)) = &above {
                self.note_parent(*parent_frame, here_frame, via);
            }
            let node = NodeRef::new(&guard);
            let kind = node.kind()?;
            if kind == PageKind::Leaf as u8 {
                if !node.covers(key)? {
                    guard.recheck()?;
                    return Err(Stop::Misrouted);
                }
                trail.pass(Level { child, resolved, guard: guard.clone() });
                return Ok(Leaf {
                    child,
                    frame: here_frame,
                    latch,
                    guard,
                    sample: resolved.sample,
                });
            }
            if kind != PageKind::Inner as u8 {
                guard.recheck()?;
                return Err(Stop::Refuse(Error::Corrupt));
            }
            if !node.covers(key)? {
                guard.recheck()?;
                return Err(Stop::Misrouted);
            }
            let (next, slot) = Self::child_of(&node, key)?;
            guard.recheck()?;
            trail.pass(Level { child, resolved, guard: guard.clone() });
            self.maybe_promote(here, here_frame);
            above = Some((guard, here_frame));
            via = slot;
            child = next;
        }
    }
    fn mark_frame_dirty(&self, index: u32, image: &[u8]) {
        self.mark_part(&Part::image(index, image));
    }
    fn mark_part(&self, part: &Part<'_>) {
        let index = part.frame;
        let Some(slot) = self.pool.slot(index) else { return };
        let _ = slot.frame.warm();
        let Some(wal) = self.wal.as_ref() else {
            slot.frame.modified(slot.frame.gsn() + 1);
            return;
        };
        let page = slot.frame.page_id().map_or(0, crate::PageId::get);
        wal.note_prior(slot.frame.gsn(), slot.frame.last_writer());
        let prev = slot.frame.gsn();
        let observed = core::cell::Cell::new(1_u64);
        let under = |want: Option<u64>| {
            let now = self.pool.captures_of(index);
            observed.set(now);
            want.is_none_or(|w| w == now)
        };
        let mut fell_back = false;
        let appended = match wal.modified_as(page, prev, part.change, Some(&|| under(part.under))) {
            Ok(Some(gsn)) => Ok(gsn),
            Ok(None) => {
                fell_back = true;
                let image = (part.image)();
                wal.modified_as(page, prev, Change::Image(&image), Some(&|| under(None)))
                    .and_then(|gsn| gsn.ok_or(Error::Corrupt))
            }
            Err(e) => Err(e),
        };
        if let Ok(gsn) = appended {
            self.count_part(if fell_back { &Change::Image(&[]) } else { &part.change });
            slot.frame.modified(gsn);
            self.pool.log_under(index, observed.get());
            if let Some(worker) = crate::wal::worker::current() {
                slot.frame.written_by(u32::from(worker));
            }
        } else {
            slot.frame.modified(slot.frame.gsn() + 1);
            wal.poison();
        }
    }
    fn refuse_if_poisoned(&self) -> Result<(), Error> {
        match self.store_stop() {
            Some(wal) if wal.is_poisoned() => Err(Error::LogPoisoned),
            None if self.unanswered.load(core::sync::atomic::Ordering::Acquire) => {
                Err(Error::Device { page: None, op: crate::DeviceOp::Write })
            }
            _ => Ok(()),
        }
    }
    fn modifying<T>(&self, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        self.refuse_if_poisoned()?;
        let outcome = f()?;
        self.refuse_if_poisoned()?;
        Ok(outcome)
    }
    fn mark_smo(&self, pages: &[(u32, &[u8])], new_root: u64) {
        let mut parts = [Part::image(0, &[]); crate::wal::record::payload::SMO_MAX_PAGES];
        if pages.len() > parts.len() {
            self.mark_together(&[], new_root, crate::wal::record::Kind::Smo, None);
            return;
        }
        for (part, &(index, image)) in parts.iter_mut().zip(pages) {
            *part = Part::image(index, image);
        }
        self.mark_together(&parts[..pages.len()], new_root, crate::wal::record::Kind::Smo, None);
    }
    fn mark_value_write(&self, parts: &[Part<'_>]) {
        self.mark_together(parts, 0, crate::wal::record::Kind::ValueWrite, None);
    }
    fn mark_change(&self, parts: &[Part<'_>], undo: TxnUndo<'_>) {
        for part in parts {
            if let Some(slot) = self.pool.slot(part.frame) {
                let _ = slot.frame.warm();
            }
        }
        self.mark_together(parts, 0, crate::wal::record::Kind::Change, Some(undo));
    }
    fn mark_together(
        &self,
        parts: &[Part<'_>],
        new_root: u64,
        kind: crate::wal::record::Kind,
        undo: Option<TxnUndo<'_>>,
    ) {
        self.mark_together_with(parts, new_root, kind, undo, &[]);
    }
    fn mark_publishing(
        &self,
        parts: &[Part<'_>],
        undo: Option<TxnUndo<'_>>,
        run: Option<blob::Staged>,
        retiring: Option<(crate::node::blob::Ref, usize)>,
    ) -> Option<u64> {
        for part in parts {
            if let Some(slot) = self.pool.slot(part.frame) {
                let _ = slot.frame.warm();
            }
        }
        match (parts, undo, run, retiring) {
            (parts, Some(undo), None, None) => self.mark_change(parts, undo),
            ([one], None, None, None) => self.mark_part(one),
            (parts, None, None, None) => self.mark_value_write(parts),
            (parts, undo, run, retiring) => {
                let kind = if undo.is_some() {
                    crate::wal::record::Kind::Change
                } else {
                    crate::wal::record::Kind::ValueWrite
                };
                let run = run.map(|r| {
                    (r.reference.first, r.pages(), r.reference.serial.to_le_bytes(), false)
                });
                let retiring = retiring.map(|(r, local)| {
                    let pages = crate::node::blob::pages_for(r.rest(local));
                    (r.first, pages, r.serial.to_le_bytes(), true)
                });
                let runs: Vec<_> = run.into_iter().chain(retiring).collect();
                self.mark_together_with(parts, 0, kind, undo, &runs);
            }
        }
        let leaf = parts.first()?;
        self.wal.as_ref()?;
        self.pool.slot(leaf.frame).map(|s| s.frame.gsn())
    }
    fn mark_together_with(
        &self,
        parts: &[Part<'_>],
        new_root: u64,
        kind: crate::wal::record::Kind,
        undo: Option<TxnUndo<'_>>,
        runs: &[(u64, u64, [u8; 8], bool)],
    ) {
        use crate::wal::record::payload::SMO_MAX_PAGES;
        let Some(wal) = self.wal.as_ref() else {
            for part in parts {
                if let Some(slot) = self.pool.slot(part.frame) {
                    slot.frame.modified(slot.frame.gsn() + 1);
                }
            }
            return;
        };
        let mut named = [(0_u64, 0_u64, Change::Image(&[])); SMO_MAX_PAGES];
        let entries = parts.len() + runs.len();
        let fits = !parts.is_empty() && entries <= named.len();
        if fits {
            for (to, (first, pages, serial, retired)) in named[parts.len()..].iter_mut().zip(runs) {
                let change = if *retired { Change::Retire(serial) } else { Change::Run(serial) };
                *to = (*first, *pages, change);
            }
        }
        for (to, part) in named.iter_mut().zip(parts) {
            let Some(slot) = self.pool.slot(part.frame) else { return };
            let page = slot.frame.page_id().map_or(0, crate::PageId::get);
            wal.note_prior(slot.frame.gsn(), slot.frame.last_writer());
            *to = (page, slot.frame.gsn(), part.change);
        }
        let named = &mut named[..entries.min(SMO_MAX_PAGES)];
        let observed = [const { core::cell::Cell::new(1_u64) }; SMO_MAX_PAGES];
        let under = |changes: bool| {
            let mut all = true;
            for (i, part) in parts.iter().enumerate().take(SMO_MAX_PAGES) {
                let now = self.pool.captures_of(part.frame);
                observed[i].set(now);
                all &= !changes || part.under.is_none_or(|w| w == now);
            }
            all
        };
        let mut appended = if fits {
            wal.modified_many_as(
                named,
                new_root,
                kind,
                Some(&|| under(true)),
                undo.map(TxnUndo::logged),
            )
        } else {
            Err(Error::Corrupt)
        };
        let images: [Vec<u8>; SMO_MAX_PAGES];
        if matches!(appended, Ok(None)) {
            images = core::array::from_fn(|i| {
                parts.get(i).filter(|p| p.change.is_op()).map_or_else(Vec::new, |p| (p.image)())
            });
            for ((to, image), part) in named.iter_mut().zip(&images).zip(parts) {
                if part.change.is_op() {
                    to.2 = Change::Image(image);
                }
            }
            appended = wal.modified_many_as(
                named,
                new_root,
                kind,
                Some(&|| under(false)),
                undo.map(TxnUndo::logged),
            );
        }
        if let Ok(Some(gsn)) = appended {
            for (i, part) in parts.iter().enumerate() {
                self.count_part(&named[i].2);
                if let Some(slot) = self.pool.slot(part.frame) {
                    slot.frame.modified(gsn);
                    self.pool.log_under(part.frame, observed[i].get());
                    if let Some(worker) = crate::wal::worker::current() {
                        slot.frame.written_by(u32::from(worker));
                    }
                }
            }
            return;
        }
        for part in parts {
            if let Some(slot) = self.pool.slot(part.frame) {
                slot.frame.modified(slot.frame.gsn() + 1);
            }
        }
        wal.poison();
    }
    fn image_of<const N: usize>(guard: &crate::latch::ExclusiveGuard<'_, Node, N>) -> Vec<u8> {
        use crate::node::offsets::{COUNT, DATA_OFFSET};
        use crate::wal::record::payload;
        let field =
            |at: usize| guard.read_bytes_unvalidated::<2>(at).map_or(u16::MAX, u16::from_le_bytes);
        let gap = payload::gap_named(field(COUNT), field(DATA_OFFSET));
        let mut image = vec![0u8; payload::image_len(gap)];
        let (prefix, body) = image.split_at_mut(payload::IMAGE_PREFIX_BYTES);
        if let Ok(bytes) = payload::image_prefix(gap) {
            prefix.copy_from_slice(&bytes);
        }
        let (head, tail) = body.split_at_mut(gap.0);
        let _ = guard.peek(0, head);
        let _ = guard.peek(gap.1, tail);
        image
    }
    fn op_under(&self, frame: u32) -> Option<u64> {
        self.wal.as_ref()?;
        let f = &self.pool.slot(frame)?.frame;
        let count = self.pool.captures_of(frame);
        (f.gsn() != 0 && count.is_multiple_of(2) && self.pool.logged(frame) == count)
            .then_some(count)
    }
    fn logged_op<const N: usize>(
        &self,
        frame: u32,
        guard: &crate::latch::ExclusiveGuard<'_, Node, N>,
        op: &Op<'_>,
    ) -> (Vec<u8>, Option<u64>) {
        use crate::node::offsets::{COUNT, DATA_OFFSET};
        use crate::wal::record::payload;
        if self.wal.is_none() {
            return (Vec::new(), None);
        }
        if let Some(count) = self.op_under(frame) {
            let field = |at: usize| {
                guard.read_bytes_unvalidated::<2>(at).map_or(u16::MAX, u16::from_le_bytes)
            };
            let mut out = Vec::new();
            if op.encode(payload::gap_named(field(COUNT), field(DATA_OFFSET)), &mut out).is_ok() {
                return (out, Some(count));
            }
        }
        (Self::image_of(guard), None)
    }
    fn image_of_node(node: &Node) -> Vec<u8> {
        crate::wal::record::payload::image_of_page(&node.to_page_bytes()).unwrap_or_default()
    }
    fn record_parent_hint(&self, parent: ChildRef, child: ChildRef, at: usize) {
        let (Some(pf), Some(cf)) = (self.frame_index(parent), self.frame_index(child)) else {
            return;
        };
        self.note_parent(pf, cf, at);
    }
    fn note_parent(&self, pf: u32, cf: u32, at: usize) {
        let (Some(ps), Some(cs)) = (self.pool.slot(pf), self.pool.slot(cf)) else {
            return;
        };
        let Ok(at) = u32::try_from(at) else { return };
        cs.frame.parent.record(pf, at, ps.frame.version());
    }
    fn child_of<const N: usize>(
        node: &NodeRef<'_, '_, N>,
        key: &[u8],
    ) -> Optimistic<(ChildRef, usize)> {
        let (at, _) = node.lower_bound(key)?;
        let n = node.len()?;
        if at >= n {
            return Ok((ChildRef::from_raw(node.upper()?), n));
        }
        let mut raw = [0_u8; 8];
        let len = node.payload_at(at, &mut raw)?;
        if len != 8 {
            return Err(Unwind);
        }
        Ok((ChildRef::from_raw(u64::from_le_bytes(raw)), at))
    }
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct Op<'a> {
    code: u8,
    flags: u16,
    key: &'a [u8],
    payload: &'a [u8],
}
impl<'a> Op<'a> {
    pub(crate) const fn put(added: bool, key: &'a [u8], payload: &'a [u8], flags: u16) -> Self {
        use crate::wal::record::payload::{OP_INSERT, OP_REPLACE};
        Op { code: if added { OP_INSERT } else { OP_REPLACE }, flags, key, payload }
    }
    pub(crate) const fn remove(key: &'a [u8]) -> Self {
        Op { code: crate::wal::record::payload::OP_REMOVE, flags: 0, key, payload: &[] }
    }
    pub(crate) fn encode(&self, gap: (usize, usize), out: &mut Vec<u8>) -> Result<(), Error> {
        crate::wal::record::payload::write_op(
            &crate::wal::record::payload::TupleOp {
                gap,
                op: self.code,
                flags: self.flags,
                key: self.key,
                payload: self.payload,
            },
            out,
        )
    }
}
#[derive(Debug, Clone, Copy)]
pub struct TxnUndo<'a> {
    pub txn: u64,
    pub part: &'a crate::wal::record::payload::UndoPart<'a>,
    pub(crate) from: Option<&'a Leaf<'a>>,
}
impl<'a> TxnUndo<'a> {
    const fn logged(self) -> (u64, &'a crate::wal::record::payload::UndoPart<'a>) {
        (self.txn, self.part)
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Part<'a> {
    pub(crate) frame: u32,
    pub(crate) change: Change<'a>,
    pub(crate) under: Option<u64>,
    pub(crate) image: &'a dyn Fn() -> Vec<u8>,
}
impl<'a> Part<'a> {
    pub(crate) fn logged(
        frame: u32,
        logged: &'a (Vec<u8>, Option<u64>),
        image: &'a dyn Fn() -> Vec<u8>,
    ) -> Self {
        let change =
            if logged.1.is_some() { Change::Op(&logged.0) } else { Change::Image(&logged.0) };
        Part { frame, change, under: logged.1, image }
    }
    pub(crate) fn image(frame: u32, image: &'a [u8]) -> Self {
        Part { frame, change: Change::Image(image), under: None, image: &NO_IMAGE }
    }
}
static NO_IMAGE: fn() -> Vec<u8> = Vec::new;
pub(crate) struct Capturing<'t> {
    pool: &'t crate::bufmgr::pool::Pool,
    of: core::ops::Range<usize>,
}
impl Drop for Capturing<'_> {
    fn drop(&mut self) {
        self.pool.capture_ends(self.of.clone());
    }
}
#[path = "tree/blob.rs"]
pub mod blob;
#[path = "tree/insert.rs"]
mod insert;
#[path = "tree/merge.rs"]
mod merge;
#[path = "tree/remove.rs"]
mod remove;
#[path = "tree/scan.rs"]
pub mod scan;
pub mod space;
#[path = "tree/value.rs"]
pub mod value;
impl Tree {
    fn make_room(&self) -> Result<Room, Error> {
        let seed = self
            .inline_seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed);
        let mut sampler = Sampler::new(seed);
        let budget =
            Budget { demand: 1, ..Budget::for_pool(self.pool.len(), self.pool.partitions()) };
        let mut undecided = false;
        for attempt in 0..5 {
            if attempt == 4 {
                let Some(wal) = self.wal.as_ref() else { break };
                wal.make_every_log_durable()?;
                self.forced_log_flushes.add(1);
            }
            let pass = self.provide(&mut sampler, budget);
            if pass.pages_evicted > 0 {
                return Ok(Room::Made);
            }
            undecided |= pass.root_unknown > 0;
        }
        let room = if self.pool.free_frames() > 0 {
            Room::Made
        } else if undecided {
            Room::Undecided
        } else {
            Room::Nothing
        };
        Ok(room)
    }
    fn with_room<T>(&self, mut attempt: impl FnMut() -> Result<T, Error>) -> Result<T, Error> {
        for _ in 0..RETRY_BOUND {
            match attempt() {
                Err(Error::Full { capacity }) => {
                    if self.make_room()? == Room::Nothing {
                        return Err(Error::Full { capacity });
                    }
                }
                done => return done,
            }
        }
        Err(Error::Livelocked { attempts: RETRY_BOUND })
    }
    pub fn write_back_all(&self) -> Counters {
        self.write_back(0..self.pool.partitions())
    }
    pub fn write_back(&self, of: core::ops::Range<usize>) -> Counters {
        let channel = crate::store::channel::BlockingChannel::new(&*self.store);
        let counters = crate::bufmgr::provider::write_back(
            &self.pool,
            of,
            self,
            &channel,
            self.wal.as_ref().map(|w| w.flushed()),
        );
        self.stop_if_unanswered(counters);
        counters
    }
    fn stop_if_unanswered(&self, counters: Counters) {
        if counters.unanswered == 0 {
            return;
        }
        match self.store_stop() {
            Some(wal) => wal.poison(),
            None => self.unanswered.store(true, core::sync::atomic::Ordering::Release),
        }
    }
    pub fn write_back_every_page(
        &self,
        of: core::ops::Range<usize>,
        make_durable: impl FnOnce() -> Result<(), Error>,
    ) -> Result<Counters, Error> {
        let flushed = self.wal.as_ref().ok_or(Error::Corrupt)?.flushed();
        let channel = crate::store::channel::BlockingChannel::new(&*self.store);
        let counters = crate::bufmgr::provider::write_back_every_page(
            &self.pool,
            of,
            self,
            &channel,
            flushed,
            make_durable,
        )?;
        self.stop_if_unanswered(counters);
        Ok(counters)
    }
    pub(crate) fn capturing(&self, of: core::ops::Range<usize>) -> Capturing<'_> {
        self.pool.capture_begins(of.clone());
        Capturing { pool: &self.pool, of }
    }
    #[must_use]
    pub fn partitions(&self) -> usize {
        self.pool.partitions()
    }
    #[must_use]
    pub fn moved_into(&self, of: core::ops::Range<usize>) -> u64 {
        self.pool.moved_into(of)
    }
    pub(crate) fn vouch_from(&self, begins: &[u64]) {
        self.pool.vouch_from(begins);
    }
    pub(crate) fn floors_covered(&self, of: core::ops::Range<usize>) {
        self.pool.floors_covered(of);
    }
    pub(crate) fn mark_clean(&self, of: core::ops::Range<usize>, captured: &[u64]) {
        self.pool.mark_clean(of, captured);
    }
    pub(crate) fn least_vouched(&self, workers: usize) -> (Vec<u64>, bool) {
        self.pool.least_vouched(workers)
    }
    pub(crate) fn enter_smo(&self) -> SmoMark<'_> {
        self.in_smo.add(1);
        SmoMark { tree: self }
    }
    #[must_use]
    pub fn evictions_during_smo(&self) -> u64 {
        self.evictions_in_smo.get()
    }
    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.evictions.get()
    }
    #[must_use]
    pub fn forced_log_flushes(&self) -> u64 {
        self.forced_log_flushes.get()
    }
    #[must_use]
    pub fn scan_hops(&self) -> u64 {
        self.scan_hops.get()
    }
    #[must_use]
    pub fn scan_chunks(&self) -> (u64, u64) {
        (self.scan_chunks.get(), self.scan_chunks_refused.get())
    }
    #[must_use]
    pub fn scan_absent_resumes(&self) -> (u64, u64) {
        (self.scan_absent.get(), self.scan_absent_after_skip.get())
    }
    #[must_use]
    pub fn leaves_reused(&self) -> (u64, u64) {
        (self.leaves_reused.get(), self.leaves_redescended.get())
    }
    #[must_use]
    pub fn yields(&self) -> u64 {
        self.yields.get()
    }
    #[must_use]
    pub fn reads_held(&self) -> (u64, u64) {
        (self.reads_held.get(), self.announcements.get())
    }
    #[must_use]
    pub fn smo_restarts(&self) -> u64 {
        self.smo_restarts.get()
    }
    #[must_use]
    pub fn merges(&self) -> u64 {
        self.merges.get()
    }
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        let Ok(holder) = self.root.exclusive() else { return 0 };
        let root = holder.get();
        drop(holder);
        let mut next = Some(root);
        let mut n = 0;
        while let Some(child) = next {
            let Ok(resolved) = self.resolve(child) else { break };
            let Ok(guard) = resolved.latch.exclusive() else { break };
            if self.confirm(&resolved).is_err() {
                break;
            }
            let node = guard.get();
            if !node.is_leaf() {
                let raw = node.payload_at(0).try_into().map_or(0, u64::from_le_bytes);
                drop(guard);
                next = (raw != 0).then(|| ChildRef::from_raw(raw));
                continue;
            }
            n += 1;
            let raw = node.right();
            drop(guard);
            next = (raw != 0).then(|| ChildRef::from_raw(raw));
            if n > self.pool.len() {
                break;
            }
        }
        n
    }
    #[must_use]
    pub fn is_acyclic(&self) -> bool {
        let Ok(root_guard) = self.root.exclusive() else {
            return true;
        };
        let root = root_guard.get();
        drop(root_guard);
        let Some(root_frame) = self.frame_index(root) else {
            return true;
        };
        let mut seen = vec![false; self.pool.len()];
        let mut stack = vec![root_frame];
        while let Some(frame) = stack.pop() {
            let at = frame as usize;
            if at >= seen.len() {
                return false;
            }
            if seen[at] {
                return false;
            }
            seen[at] = true;
            let Ok(count) = self.child_count(frame) else { continue };
            for slot in 0..count {
                match self.child_at(frame, slot) {
                    Ok(Some(child)) => {
                        if let Some(child) = self.frame_index(child) {
                            stack.push(child);
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
        true
    }
    pub fn provide(&self, sampler: &mut Sampler, budget: Budget) -> Counters {
        let channel = crate::store::channel::BlockingChannel::new(&*self.store);
        let counters = crate::bufmgr::provider::run_once(
            &self.pool,
            self,
            &channel,
            sampler,
            budget,
            self.wal.as_ref().map(|w| w.flushed()),
        );
        self.stop_if_unanswered(counters);
        if counters.pages_evicted > 0 {
            let n = counters.pages_evicted;
            self.evictions.add(n);
            if self.in_smo.get() > 0 {
                self.evictions_in_smo.add(n);
            }
        }
        counters
    }
}
impl Children for Tree {
    fn resident_frame(&self, _pool: &Pool, child: ChildRef) -> Optimistic<Option<u32>> {
        let Some(page) = child.page_id() else { return Ok(None) };
        self.translation.lookup(page)
    }
    fn forget(&self, page: PageId, frame: u32) {
        let _ = self.translation.remove_named(page, frame);
    }
    fn child_at(&self, frame: u32, slot: u32) -> Optimistic<Option<ChildRef>> {
        let s = self.pool.slot(frame).ok_or(Unwind)?;
        let guard = s.page.optimistic()?;
        let node = NodeRef::new(&guard);
        if !node.is_inner()? {
            return Ok(None);
        }
        let n = node.len()?;
        if slot as usize > n {
            return Ok(None);
        }
        if slot as usize == n {
            let upper = node.upper()?;
            guard.recheck()?;
            return Ok(if upper == 0 { None } else { Some(ChildRef::from_raw(upper)) });
        }
        let mut raw = [0_u8; 8];
        if node.payload_at(slot as usize, &mut raw)? != 8 {
            return Err(Unwind);
        }
        guard.recheck()?;
        Ok(Some(ChildRef::from_raw(u64::from_le_bytes(raw))))
    }
    fn is_root(&self, frame: u32) -> Optimistic<bool> {
        let root = self.root.optimistic()?.read()?;
        Ok(Children::resident_frame(self, &self.pool, root)? == Some(frame))
    }
    fn child_count(&self, frame: u32) -> Optimistic<u32> {
        let s = self.pool.slot(frame).ok_or(Unwind)?;
        let guard = s.page.optimistic()?;
        let node = NodeRef::new(&guard);
        if !node.is_inner()? {
            return Ok(0);
        }
        let n = node.len()? + usize::from(node.upper()? != 0);
        guard.recheck()?;
        u32::try_from(n).map_err(|_| Unwind)
    }
}
impl core::fmt::Debug for Tree {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tree").field("root", &self.root.version()).finish_non_exhaustive()
    }
}
pub(crate) struct SmoMark<'a> {
    tree: &'a Tree,
}
impl Drop for SmoMark<'_> {
    fn drop(&mut self) {
        self.tree.in_smo.sub(1);
    }
}
