use crate::Error;
use crate::store::PageStore;
use crate::wal::worker::{Bound, Wal, bind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
pub const LOG_BYTES: usize = 1 << 24;
pub const LOG_DEVICE_BYTES: usize = 16 * LOG_BYTES;
pub const LOG_LIMIT_BYTES: u64 = 20 << 30;
pub fn log_device_ring_for(workers: usize) -> Result<usize, Error> {
    let share = LOG_LIMIT_BYTES / workers.max(1) as u64;
    let step = LOG_BYTES as u64;
    let ring = (share.saturating_mul(2) / step * step).max(LOG_DEVICE_BYTES as u64);
    usize::try_from(ring).map_err(|_| Error::RingTooLarge { bytes: ring })
}
fn log_memory_for(device: usize) -> usize {
    if device > LOG_BYTES && device.is_multiple_of(LOG_BYTES) { LOG_BYTES } else { device }
}
const RELEASE_BOUND: u32 = 4096;
const WRITE_DECISIONS: u32 = 64;
#[derive(Debug, Clone, Copy)]
enum Wrote {
    Written(Undo),
    Present,
    Moved,
}
fn stopping(wal: &Wal, workers: usize) -> Arc<Commits> {
    let commits = Arc::new(Commits::new(workers));
    let woken = Arc::clone(&commits);
    let _ = wal.on_stop(Box::new(move || {
        let _ = woken.ring_all();
    }));
    commits
}
const LOG_BASE: u64 = 1;
const CHECKPOINT_ROUNDS: usize = 64;
const CHECKPOINT_ROOT_ATTEMPTS: u32 = 64;
const REFUSAL_CHECKPOINTS: usize = 2;
const INCREMENTS: usize = 10;
fn collect_bound_at(oldest: Option<Timestamp>, floor: Timestamp) -> Result<Timestamp, Error> {
    Timestamp::new(calc::collect_bound(oldest.map(Timestamp::get), floor.get()))
        .ok_or(Error::Corrupt)
}
fn paced_rings(workers: usize) -> Box<[crate::sync::Padded<PacedRing>]> {
    (0..workers)
        .map(|_| {
            crate::sync::Padded(PacedRing {
                from: AtomicU64::new(u64::MAX),
                crossed: AtomicU64::new(0),
                paid: AtomicU64::new(0),
            })
        })
        .collect()
}
use crate::tree::scan::Local;
use crate::txn::clock::Clock;
use crate::txn::commits::Commits;
use crate::txn::versions::Versions;
use crate::txn::visible::{Dependencies, Reader, Seen, head_visible, visible};
use crate::txn::{Snapshots, Timestamp, Watermark, WorkerId, tuple};
use crate::{Tree, tree::scan::Step};
struct Gate;
impl Gate {
    const fn none() -> Self {
        Self
    }
}
fn gated(_: &Wal, _: Gate) {}
#[derive(Default)]
pub struct Stores {
    pub data: Option<Box<dyn PageStore>>,
    pub versions: Option<Box<dyn PageStore>>,
    pub log: Option<Box<dyn PageStore>>,
    pub catalog: Option<Box<dyn PageStore>>,
    pub reads: Option<Box<dyn crate::store::channel::IoChannel>>,
}
impl core::fmt::Debug for Stores {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let on = |s: &Option<Box<dyn PageStore>>| {
            if s.is_some() { "device" } else { "memory" }
        };
        f.debug_struct("Stores")
            .field("data", &on(&self.data))
            .field("versions", &on(&self.versions))
            .field("log", &on(&self.log))
            .field("catalog", &on(&self.catalog))
            .field("reads", &if self.reads.is_some() { "channel" } else { "store" })
            .finish()
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    #[default]
    Durable,
    Eventual,
}
pub const EVENTUAL_BOUND: u64 = 1 << 20;
pub struct Db {
    tree: Tree,
    wal: Arc<Wal>,
    clock: Clock,
    snapshots: Snapshots,
    commits: Arc<Commits>,
    versions: Versions,
    catalog: Option<Arc<crate::catalog::Keeper>>,
    checkpointing: crate::sync::Mutex<Checkpointer>,
    ready_deps: crate::stats::Stat,
    read_only_commits: crate::stats::Stat,
    removals: Box<[crate::sync::Mutex<Removals>]>,
    undo_held: crate::stats::Stat,
    ack_wait_turns: crate::stats::Stat,
    helped_acks: crate::stats::Stat,
    pacing: Arc<Pacing>,
    paced: Box<[crate::sync::Padded<PacedRing>]>,
    scan_rows_decided: crate::stats::Stat,
    scan_rows_resolved: crate::stats::Stat,
    releases: crate::stats::Stat,
    release_yields: crate::stats::Stat,
    write_redecisions: crate::stats::Stat,
    dep_writes: crate::stats::Stat,
    idle_writes: crate::stats::Stat,
    barrier_txns: crate::stats::Stat,
    collecting_skips: crate::stats::Stat,
    durability: Durability,
    eventual_bound: u64,
}
thread_local! {
    static OBSERVED : core::cell::RefCell < Vec < (u16, crate ::wal::worker::Written) >>
    = const { core::cell::RefCell::new(Vec::new()) };
}
#[derive(Debug, Default)]
struct ScanBuffers {
    from: Vec<u8>,
    next: Vec<u8>,
}
std::thread_local! {
    static SCAN_BUFFERS : core::cell::Cell < ScanBuffers > =
    core::cell::Cell::new(ScanBuffers::default());
}
struct LentScan(ScanBuffers);
impl LentScan {
    fn take() -> Self {
        Self(SCAN_BUFFERS.with(core::cell::Cell::take))
    }
}
impl Drop for LentScan {
    fn drop(&mut self) {
        let buffers = core::mem::take(&mut self.0);
        let _ = SCAN_BUFFERS.try_with(|slot| slot.set(buffers));
    }
}
#[derive(Debug, Default)]
pub(crate) struct Pacing {
    state: crate::sync::Mutex<PaceState>,
    due: crate::sync::Condvar,
    done: crate::sync::Condvar,
}
#[derive(Debug, Default)]
struct PaceState {
    target: u64,
    pending: bool,
    started: u64,
    finished: u64,
    threads: usize,
    stop: bool,
}
#[derive(Debug)]
struct PacedRing {
    from: AtomicU64,
    crossed: AtomicU64,
    paid: AtomicU64,
}
fn checkpointer(db: &std::sync::Weak<Db>, pacing: &Pacing) {
    let _detach = Detach(pacing);
    loop {
        {
            let Ok(mut s) = pacing.state.lock() else { return };
            while !s.stop && !s.pending && s.started >= s.target {
                match pacing.due.wait(s) {
                    Ok(next) => s = next,
                    Err(_) => return,
                }
            }
            if s.stop {
                return;
            }
            s.pending = false;
            s.started += 1;
        }
        let Some(db) = db.upgrade() else { return };
        db.paced_increment();
        let owed = db.owed();
        drop(db);
        if let Ok(mut s) = pacing.state.lock() {
            s.finished += 1;
            s.pending |= owed;
        }
        pacing.done.notify_all();
    }
}
struct Detach<'p>(&'p Pacing);
impl Drop for Detach<'_> {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.threads = s.threads.saturating_sub(1);
        drop(s);
        self.0.done.notify_all();
    }
}
impl Drop for Db {
    fn drop(&mut self) {
        self.stop_checkpointer();
    }
}
const COLLECT_FLOOR: usize = 64;
#[derive(Debug, Clone, Copy)]
enum Covering {
    Pool,
    Next,
}
#[derive(Debug)]
struct Checkpointer {
    counts: Checkpoints,
    next: usize,
}
impl Checkpointer {
    fn starting_at(tree: &Tree, begins: &[u64]) -> Self {
        tree.vouch_from(begins);
        Self { counts: Checkpoints::default(), next: 0 }
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoints {
    pub taken: u64,
    pub unclean: u64,
    pub partial: u64,
    pub held_by_moves: u64,
    pub deferred: u64,
}
impl core::fmt::Debug for Db {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Db")
            .field("tree", &self.tree)
            .field("wal", &self.wal)
            .field("clock", &self.clock)
            .field("snapshots", &self.snapshots)
            .field("commits", &self.commits)
            .field("versions", &self.versions)
            .field("catalog", &if self.catalog.is_some() { "device" } else { "none" })
            .field("checkpointing", &self.checkpointing)
            .field("ready_deps", &self.ready_deps)
            .field("read_only_commits", &self.read_only_commits)
            .field("removals", &self.removals.len())
            .field("undo_held", &self.undo_held)
            .field("ack_wait_turns", &self.ack_wait_turns)
            .field("helped_acks", &self.helped_acks)
            .field("pacing", &self.pacing)
            .field("paced", &self.paced)
            .field("scan_rows_decided", &self.scan_rows_decided)
            .field("scan_rows_resolved", &self.scan_rows_resolved)
            .field("releases", &self.releases)
            .field("release_yields", &self.release_yields)
            .field("write_redecisions", &self.write_redecisions)
            .field("dep_writes", &self.dep_writes)
            .field("idle_writes", &self.idle_writes)
            .field("barrier_txns", &self.barrier_txns)
            .field("collecting_skips", &self.collecting_skips)
            .field("durability", &self.durability)
            .field("eventual_bound", &self.eventual_bound)
            .finish()
    }
}
impl Db {
    pub fn with_capacity(workers: usize, capacity: usize) -> Result<Self, Error> {
        Self::with_stores(workers, capacity, Stores::default())
    }
    pub fn with_stores(workers: usize, capacity: usize, stores: Stores) -> Result<Self, Error> {
        Self::with_log_capacity(workers, capacity, log_device_ring_for(workers)?, stores)
    }
    pub fn with_log_capacity(
        workers: usize,
        capacity: usize,
        log_bytes: usize,
        stores: Stores,
    ) -> Result<Self, Error> {
        Self::create(workers, capacity, (None, log_bytes), stores, Gate::none())
    }
    pub fn with_log_sizes(
        workers: usize,
        capacity: usize,
        memory: usize,
        device: usize,
        stores: Stores,
    ) -> Result<Self, Error> {
        Self::create(workers, capacity, (Some(memory), device), stores, Gate::none())
    }
    fn create(
        workers: usize,
        capacity: usize,
        (memory, log_bytes): (Option<usize>, usize),
        stores: Stores,
        gate: Gate,
    ) -> Result<Self, Error> {
        let Stores { data, versions, log, catalog, reads: _ } = stores;
        if catalog.is_some() && log.is_none() {
            return Err(Error::CatalogWithoutLog);
        }
        if catalog.is_some() && workers > crate::catalog::MAX_WORKERS {
            return Err(Error::TooManyWorkers {
                requested: workers,
                most: crate::catalog::MAX_WORKERS,
            });
        }
        let log_bytes = if log.is_some() {
            log_bytes.div_ceil(crate::PAGE_SIZE).max(1) * crate::PAGE_SIZE
        } else {
            log_bytes
        };
        let memory = memory.unwrap_or_else(|| log_memory_for(log_bytes));
        if memory % crate::PAGE_SIZE != 0 && log.is_some() {
            return Err(Error::Corrupt);
        }
        let wal = Wal::staged(workers, memory, log_bytes)?;
        let wal = Arc::new(match log {
            Some(device) => wal.on_device(device, LOG_BASE)?,
            None => wal,
        });
        gated(&wal, gate);
        let catalog = catalog.map(|device| Arc::new(crate::catalog::Keeper::new(device, 0)));
        let _bound = bind(0);
        if catalog.is_some() {
            wal.retain_everything()?;
        }
        let data = match data {
            Some(store) => store,
            None => Tree::default_store(capacity),
        };
        let mut tree = Tree::with_store_logged(capacity, data, Arc::clone(&wal))?;
        tree.set_keep_local(tuple::HEAD_BYTES);
        let checkpointer = Checkpointer::starting_at(&tree, &vec![0; workers]);
        let commits = stopping(&wal, workers);
        let versions = Versions::of_store(workers, capacity, versions, &wal)?;
        let db = Self {
            tree,
            wal,
            clock: Clock::new(),
            snapshots: Snapshots::new(workers),
            commits,
            versions,
            catalog,
            checkpointing: crate::sync::Mutex::new(checkpointer),
            ready_deps: crate::stats::Stat::default(),
            read_only_commits: crate::stats::Stat::default(),
            removals: (0..workers).map(|_| crate::sync::Mutex::new(Removals::default())).collect(),
            undo_held: crate::stats::Stat::default(),
            ack_wait_turns: crate::stats::Stat::default(),
            helped_acks: crate::stats::Stat::default(),
            pacing: Arc::new(Pacing::default()),
            paced: paced_rings(workers),
            scan_rows_decided: crate::stats::Stat::default(),
            scan_rows_resolved: crate::stats::Stat::default(),
            releases: crate::stats::Stat::default(),
            release_yields: crate::stats::Stat::default(),
            write_redecisions: crate::stats::Stat::default(),
            dep_writes: crate::stats::Stat::default(),
            idle_writes: crate::stats::Stat::default(),
            barrier_txns: crate::stats::Stat::default(),
            collecting_skips: crate::stats::Stat::default(),
            durability: Durability::Durable,
            eventual_bound: EVENTUAL_BOUND,
        };
        if let Some(catalog) = db.catalog.as_ref() {
            db.checkpoint()?;
            db.wal.keep_below(Arc::clone(catalog));
        }
        Ok(db)
    }
    pub fn open(workers: usize, capacity: usize, stores: Stores) -> Result<Self, Error> {
        Self::reopen(workers, capacity, None, stores, Gate::none())
    }
    pub fn open_staged(
        workers: usize,
        capacity: usize,
        memory: usize,
        stores: Stores,
    ) -> Result<Self, Error> {
        Self::reopen(workers, capacity, Some(memory), stores, Gate::none())
    }
    fn reopen(
        workers: usize,
        capacity: usize,
        memory: Option<usize>,
        stores: Stores,
        gate: Gate,
    ) -> Result<Self, Error> {
        let Stores { data, versions, log, catalog, reads } = stores;
        let catalog_device = catalog.ok_or(Error::Corrupt)?;
        if log.is_none() {
            return Err(Error::CatalogWithoutLog);
        }
        let recorded = crate::catalog::load(catalog_device.as_ref())?;
        let catalog = Arc::new(crate::catalog::Keeper::new(catalog_device, recorded.stamp_ceiling));
        if recorded.workers.len() != workers {
            return Err(Error::WorkerCountMismatch {
                recorded: recorded.workers.len(),
                requested: workers,
            });
        }
        let log_bytes = usize::try_from(recorded.ring_capacity).map_err(|_| Error::Corrupt)?;
        let wal =
            Wal::staged(workers, memory.unwrap_or_else(|| log_memory_for(log_bytes)), log_bytes)?;
        let wal = Arc::new(match log {
            Some(device) => wal.reopen(device, recorded.ring_base)?,
            None => wal,
        });
        gated(&wal, gate);
        if let Some(device) = wal.device() {
            device.barrier()?;
        }
        wal.keep_below(Arc::clone(&catalog));
        let mut issued_after_redo = None;
        let replayed = if let (Some(_), Some(store)) = (wal.device(), data.as_deref()) {
            let len = wal.ring_device_bytes();
            if len == 0 {
                return Err(Error::Corrupt);
            }
            let mut starts = Vec::with_capacity(workers);
            for worker in 0..workers {
                let begin = recorded.workers.get(worker).ok_or(Error::Corrupt)?.begin;
                starts.push((begin, begin / len));
            }
            let rings: Arc<dyn crate::recovery::Rings> =
                Arc::new(crate::recovery::WriteBackAsRead::new(Arc::clone(&wal)));
            let mut analysis = crate::recovery::analyse_log(&rings, &starts)?;
            analysis.checkpoint_floor =
                recorded.workers.iter().map(|w| w.flushed).min().unwrap_or(0);
            for ring in &analysis.rings {
                if let crate::wal::scan::Stop::StaleLap { expected, found } = ring.stop
                    && found > expected
                {
                    return Err(Error::RecoveryFailed);
                }
            }
            if let Some(device) = wal.device() {
                device.barrier()?;
            }
            for (worker, ring) in analysis.rings.iter().enumerate() {
                let begin = recorded.workers.get(worker).ok_or(Error::Corrupt)?.begin;
                wal.resume_ring(worker, ring.end, ring.last_gsn, begin)?;
            }
            store.resume_issued(recorded.highest_page)?;
            let (applied, after_redo, effective) = match reads.as_deref() {
                Some(channel) => crate::recovery::redo_reading(&analysis, store, channel)?,
                None => crate::recovery::redo_reporting(&analysis, store)?,
            };
            analysis.after_redo = after_redo;
            analysis.effective = effective;
            issued_after_redo = Some(store.len() as u64);
            crate::recovery::decide(&mut analysis, store);
            crate::recovery::honour_named(&mut analysis);
            let root = crate::recovery::recovered_root(&analysis, store);
            Some((analysis, applied, root))
        } else {
            if let Some(store) = data.as_deref() {
                store.resume_issued(recorded.highest_page)?;
            }
            None
        };
        let _ = &replayed;
        drop(reads);
        let recovered = replayed.as_ref().map_or(0, |(analysis, _, _)| analysis.highest_gsn);
        wal.resume_clocks(recovered.max(recorded.gsn_clock));
        wal.raise_clocks(recorded.stamp_ceiling);
        let root = replayed.as_ref().and_then(|&(_, _, root)| root).unwrap_or(recorded.root);
        let mut tree = match data {
            Some(store) => Tree::open(capacity, store, root)?,
            None => return Err(Error::Corrupt),
        }
        .with_wal(Arc::clone(&wal));
        tree.set_keep_local(tuple::HEAD_BYTES);
        tree.resume_blob_serials(recorded.blob_serial);
        if let Some((analysis, _, _)) = &replayed {
            tree.resume_blob_serials(analysis.highest_blob_serial);
        }
        {
            let mut candidates = crate::catalog::load_free(catalog.device(), &recorded);
            let redone = replayed.as_ref().map(|(analysis, _, _)| &analysis.after_redo);
            if let Some((analysis, _, _)) = &replayed {
                candidates.extend(analysis.named_pages());
            }
            tree.reissue_absorbed(&candidates, root, redone);
        }
        let mut held = 0;
        let mut restored = Vec::new();
        if let Some((analysis, _, _)) = &replayed {
            let undone = crate::recovery::undo_counted(analysis, &tree)?;
            held = undone.held;
            restored = undone.runs;
            wal.make_every_log_durable()?;
        }
        if let (Some((analysis, _, _)), Some(issued)) = (&replayed, issued_after_redo) {
            let listed = crate::catalog::load_runs(catalog.device(), &recorded);
            let named: std::collections::BTreeSet<u64> =
                analysis.named_pages().map(|(page, _)| page).collect();
            let reclaim = recorded.data_pages.saturating_add(1)..issued.saturating_add(1);
            let free = crate::tree::space::rebuild(
                &listed,
                &analysis.blob_events(),
                &restored,
                reclaim,
                &named,
            );
            tree.seed_blob_space(&free, wal.max_gsn().saturating_add(1));
        }
        let highest_txn = replayed.as_ref().map_or(0, |(analysis, _, _)| analysis.highest_txn);
        let begins: Vec<u64> = recorded.workers.iter().map(|w| w.begin).collect();
        let checkpointer = Checkpointer::starting_at(&tree, &begins);
        let commits = stopping(&wal, workers);
        let versions = Versions::of_store(workers, capacity, versions, &wal)?;
        let db = Self {
            tree,
            wal,
            clock: Clock::starting_at(recorded.txn_clock.max(highest_txn).saturating_add(1)),
            snapshots: Snapshots::new(workers),
            commits,
            versions,
            catalog: Some(catalog),
            checkpointing: crate::sync::Mutex::new(checkpointer),
            ready_deps: crate::stats::Stat::default(),
            read_only_commits: crate::stats::Stat::default(),
            removals: (0..workers).map(|_| crate::sync::Mutex::new(Removals::default())).collect(),
            undo_held: crate::stats::Stat::default(),
            ack_wait_turns: crate::stats::Stat::default(),
            helped_acks: crate::stats::Stat::default(),
            pacing: Arc::new(Pacing::default()),
            paced: paced_rings(workers),
            scan_rows_decided: crate::stats::Stat::default(),
            scan_rows_resolved: crate::stats::Stat::default(),
            releases: crate::stats::Stat::default(),
            release_yields: crate::stats::Stat::default(),
            write_redecisions: crate::stats::Stat::default(),
            dep_writes: crate::stats::Stat::default(),
            idle_writes: crate::stats::Stat::default(),
            barrier_txns: crate::stats::Stat::default(),
            collecting_skips: crate::stats::Stat::default(),
            durability: Durability::Durable,
            eventual_bound: EVENTUAL_BOUND,
        };
        db.undo_held.add(held as u64);
        if let Some(below) = Timestamp::new(db.clock.peek().saturating_sub(1)) {
            for worker in 0..workers {
                let _ = db.commits.commit(WorkerId(worker), below, &db.clock, 0)?;
                db.commits.ack(WorkerId(worker), below.get())?;
            }
        }
        if replayed.is_some() {
            db.checkpoint()?;
        }
        Ok(db)
    }
    #[must_use]
    pub fn catalog(&self) -> Option<&dyn PageStore> {
        self.catalog.as_deref().map(crate::catalog::Keeper::device)
    }
    fn refuse_if_stopped(&self) -> Result<(), Error> {
        if self.wal.is_poisoned() { Err(Error::LogPoisoned) } else { Ok(()) }
    }
    pub fn checkpoint(&self) -> Result<(), Error> {
        self.increment(Covering::Pool)
    }
    pub fn checkpoint_increment(&self) -> Result<(), Error> {
        self.increment(Covering::Next)
    }
    pub fn start_checkpointer(self: &Arc<Self>) -> Result<(), Error> {
        if !self.attach_checkpointer()? {
            return Ok(());
        }
        let db = Arc::downgrade(self);
        let pacing = Arc::clone(&self.pacing);
        let refused = false;
        let spawned = !refused
            && std::thread::Builder::new()
                .name("waraq-checkpointer".into())
                .spawn(move || checkpointer(&db, &pacing))
                .is_ok();
        if spawned {
            return Ok(());
        }
        if let Ok(mut s) = self.pacing.state.lock() {
            s.threads = s.threads.saturating_sub(1);
        }
        self.pacing.done.notify_all();
        Err(Error::ThreadRefused)
    }
    pub fn attach_checkpointer(&self) -> Result<bool, Error> {
        if self.catalog.is_none() {
            return Err(Error::Corrupt);
        }
        let mut s = self.pacing.state.lock().map_err(|_| Error::Poisoned)?;
        if s.threads > 0 {
            return Ok(false);
        }
        s.threads = 1;
        Ok(true)
    }
    pub fn run_checkpointer(self: &Arc<Self>) {
        checkpointer(&Arc::downgrade(self), &self.pacing);
    }
    pub fn stop_checkpointer(&self) {
        if let Ok(mut s) = self.pacing.state.lock() {
            s.stop = true;
        }
        self.pacing.due.notify_all();
        self.pacing.done.notify_all();
    }
    fn pace(&self, worker: usize) {
        let (Some((written, capacity)), Some(ring)) =
            (self.wal.ring_position(worker), self.paced.get(worker))
        else {
            return;
        };
        let step = (capacity / 2 / INCREMENTS as u64).max(1);
        let mut from = ring.0.from.load(Ordering::Acquire);
        if from == u64::MAX {
            from = written;
            ring.0.from.store(from, Ordering::Release);
        }
        let crossed = written.saturating_sub(from) / step;
        ring.0.crossed.store(crossed, Ordering::Release);
        if crossed > ring.0.paid.load(Ordering::Acquire) {
            self.ask();
        }
    }
    fn owed(&self) -> bool {
        self.paced
            .iter()
            .any(|r| r.0.crossed.load(Ordering::Acquire) > r.0.paid.load(Ordering::Acquire))
    }
    fn ask(&self) {
        let threaded = {
            let Ok(mut s) = self.pacing.state.lock() else { return };
            if s.stop {
                return;
            }
            s.pending = true;
            s.threads > 0
        };
        if threaded {
            self.pacing.due.notify_one();
            return;
        }
        while self.owed() {
            self.paced_increment();
        }
    }
    fn paced_increment(&self) {
        for ring in &*self.paced {
            let crossed = ring.0.crossed.load(Ordering::Acquire);
            let _ = ring.0.paid.try_update(Ordering::AcqRel, Ordering::Acquire, |paid| {
                (paid < crossed).then_some(paid + 1)
            });
        }
        let _ = self.checkpoint_increment();
    }
    fn room_attempts(&self) -> usize {
        let threaded = self.pacing.state.lock().is_ok_and(|s| s.threads > 0);
        if threaded { REFUSAL_CHECKPOINTS * INCREMENTS } else { REFUSAL_CHECKPOINTS }
    }
    fn make_room(&self) {
        let Ok(mut s) = self.pacing.state.lock() else { return };
        if s.stop {
            return;
        }
        if s.threads == 0 {
            drop(s);
            let _ = self.checkpoint();
            return;
        }
        let want = s.started + 1;
        s.target = s.target.max(want);
        self.pacing.due.notify_one();
        while s.finished < want && !s.stop && s.threads > 0 {
            match self.pacing.done.wait(s) {
                Ok(next) => s = next,
                Err(_) => return,
            }
        }
        if s.threads == 0 && s.finished < want && !s.stop {
            drop(s);
            let _ = self.checkpoint();
        }
    }
    fn make_every_log_durable(&self) -> Result<(), Error> {
        self.wal.make_every_log_durable()?;
        if self.durability == Durability::Eventual {
            for worker in 0..self.workers() {
                self.harden_covered(WorkerId(worker))?;
            }
        }
        Ok(())
    }
    fn increment(&self, covering: Covering) -> Result<(), Error> {
        let catalog = self.catalog.as_deref().ok_or(Error::Corrupt)?;
        let mut serial = self.checkpointing.lock().map_err(|_| Error::Poisoned)?;
        self.refuse_if_stopped()?;
        let partitions = self.tree.partitions();
        let (of, next) = match covering {
            Covering::Pool => (0..partitions, serial.next),
            Covering::Next => {
                let (start, end, next) = calc::increment_range(serial.next, partitions, INCREMENTS);
                (start..end, next)
            }
        };
        let (root_at, positions) = {
            let _capturing = self.tree.capturing(of.clone());
            let mut attempts = 0_u32;
            loop {
                let before = self.tree.root_page()?;
                let positions = self.wal.positions()?;
                if self.tree.root_page()? == before {
                    break (before, positions);
                }
                attempts += 1;
                if attempts >= CHECKPOINT_ROOT_ATTEMPTS {
                    return Err(Error::Livelocked { attempts });
                }
            }
        };
        serial.next = next;
        let captured: Vec<u64> = (0..self.workers())
            .map(|w| positions[w].min(self.wal.live_from(w).unwrap_or(u64::MAX)))
            .collect();
        self.tree.floors_covered(of.clone());
        self.make_every_log_durable()?;
        let mut clean = false;
        let mut deferred = false;
        for _ in 0..CHECKPOINT_ROUNDS {
            let pass =
                self.tree.write_back_every_page(of.clone(), || self.make_every_log_durable())?;
            deferred |= pass.deferred > 0;
            if pass.wrote_everything() {
                clean = true;
                break;
            }
        }
        if let Err(e) = self.tree.store().barrier() {
            self.wal.poison();
            return Err(e);
        }
        let mut priors: Vec<u64> = Vec::new();
        let mut floored = false;
        catalog.write(|previous| {
            let prior = |w: usize| previous.and_then(|c| c.workers.get(w)).map_or(0, |x| x.begin);
            priors = (0..self.workers()).map(prior).collect();
            let stride = self.workers();
            if clean {
                self.tree.mark_clean(of.clone(), &captured);
            }
            let (least, held) = self.tree.least_vouched(stride);
            floored = clean && held;
            let begins: Vec<u64> = (0..stride)
                .map(|w| {
                    let Some(&vouched) = least.get(w).filter(|_| clean) else { return prior(w) };
                    vouched.max(prior(w))
                })
                .collect();
            let workers = (0..self.workers())
                .map(|w| crate::catalog::Worker {
                    begin: begins[w],
                    flushed: self.wal.flushed().of(w),
                    committed: 0,
                })
                .collect();
            let recorded = crate::catalog::Catalog {
                sequence: previous.map_or(0, |c| c.sequence).saturating_add(1),
                ring_base: LOG_BASE,
                ring_capacity: self.wal.ring_capacity() as u64,
                root: root_at.map_or(0, crate::PageId::get),
                highest_page: self.tree.store().len() as u64,
                txn_clock: self.clock.peek(),
                gsn_clock: (0..self.workers()).map(|w| self.wal.flushed().of(w)).max().unwrap_or(0),
                data_pages: self.tree.store().len() as u64,
                free_pages: 0,
                stamp_ceiling: previous.map_or_else(
                    || self.wal.max_gsn().saturating_add(crate::catalog::STAMP_CEILING_STEP),
                    |c| c.stamp_ceiling,
                ),
                blob_serial: self.tree.blob_serials_issued(),
                blob_free_pages: 0,
                workers,
            };
            self.refuse_if_stopped()?;
            let begins_recorded: Vec<u64> = recorded.workers.iter().map(|w| w.begin).collect();
            let runs = self.tree.blob_space_listing(&begins_recorded);
            Ok((recorded, crate::catalog::Free { ids: self.tree.reissuable(), runs }))
        })?;
        serial.counts.taken += 1;
        serial.counts.unclean += u64::from(!clean);
        serial.counts.partial += u64::from(of.len() < partitions);
        serial.counts.held_by_moves += u64::from(floored);
        serial.counts.deferred += u64::from(deferred);
        for (w, &prior) in priors.iter().enumerate() {
            self.wal.release_before(w, prior)?;
        }
        Ok(())
    }
    pub fn checkpoints(&self) -> Result<Checkpoints, Error> {
        self.checkpointing.lock().map(|c| c.counts).map_err(|_| Error::Poisoned)
    }
    #[must_use]
    pub fn workers(&self) -> usize {
        self.snapshots.workers()
    }
    #[must_use]
    pub fn with_separate_above(mut self, line: usize) -> Self {
        self.tree.set_separate_above(line);
        self.versions.set_separate_above(line);
        self.wal.separate_above(line);
        self
    }
    #[must_use]
    pub fn with_channels(
        mut self,
        channels: Vec<Box<dyn crate::store::channel::IoChannel>>,
    ) -> Self {
        self.tree.set_channels(channels);
        self
    }
    #[must_use]
    pub fn scan_rows(&self) -> (u64, u64) {
        (self.scan_rows_decided.get(), self.scan_rows_resolved.get())
    }
    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }
    #[must_use]
    pub fn versions(&self) -> &Versions {
        &self.versions
    }
    #[must_use]
    pub fn commits(&self) -> &Commits {
        &self.commits
    }
    #[must_use]
    pub fn snapshots(&self) -> &Snapshots {
        &self.snapshots
    }
    #[must_use]
    pub fn wal(&self) -> &Arc<Wal> {
        &self.wal
    }
    fn idle(&self, worker: WorkerId) -> bool {
        self.snapshots.is_idle(worker) && !self.wal.awaiting_ack(worker.0)
    }
    fn quiet(&self, worker: WorkerId) -> bool {
        if self.wal.is_collecting(worker.0) {
            self.collecting_skips.add(1);
            return false;
        }
        self.idle(worker)
    }
    pub fn release(
        &self,
        worker: WorkerId,
        entry: &crate::wal::driver::Precommitted,
    ) -> Result<(), Error> {
        for _ in 0..RELEASE_BOUND {
            if self.wal.releasable(entry)? {
                self.wal.mine()?.queue_mut()?.retain(|queued| queued.txn != entry.txn);
                self.releases.add(1);
                return Ok(());
            }
            OBSERVED.with(|observed| -> Result<(), Error> {
                let mut observed = observed.borrow_mut();
                observed.clear();
                let mine = u16::try_from(worker.0).map_err(|_| Error::Corrupt)?;
                observed.push((mine, self.wal.write_out(true)?.1));
                for &(writer, gsn) in &entry.deps {
                    let other = WorkerId(writer as usize);
                    if other == worker || self.wal.flushed().of(writer as usize) >= gsn {
                        continue;
                    }
                    let index = u16::try_from(writer).unwrap_or(u16::MAX);
                    let _bound = bind(index);
                    observed.push((index, self.wal.write_out(true)?.1));
                    self.dep_writes.add(1);
                }
                for index in 0..self.wal.workers() {
                    let other = WorkerId(index);
                    if other == worker || !self.quiet(other) {
                        continue;
                    }
                    let index = u16::try_from(index).unwrap_or(u16::MAX);
                    let _bound = bind(index);
                    observed.push((index, self.wal.write_out(true)?.1));
                    self.idle_writes.add(1);
                }
                self.wal.sync()?;
                for &(index, written) in observed.iter() {
                    self.wal.mark_durable(index, written);
                }
                Ok(())
            })?;
            for index in 0..self.wal.workers() {
                let other = WorkerId(index);
                if other == worker || !self.idle(other) {
                    continue;
                }
                let _bound = bind(u16::try_from(index).unwrap_or(u16::MAX));
                let _ = self.wal.barrier_transaction()?;
                self.barrier_txns.add(1);
            }
            if self.wal.releasable(entry)? {
                self.wal.mine()?.queue_mut()?.retain(|queued| queued.txn != entry.txn);
                self.releases.add(1);
                return Ok(());
            }
            self.release_yields.add(1);
            std::thread::yield_now();
        }
        Err(Error::Livelocked { attempts: RELEASE_BOUND })
    }
    fn confirm_read(
        &self,
        durability: Durability,
        writer: WorkerId,
        tag: u64,
    ) -> Result<(), Error> {
        if durability == Durability::Eventual && self.commits.is_answered(writer, tag)? {
            return Ok(());
        }
        self.wait_acked(writer, tag)
    }
    pub(crate) fn wait_acked(&self, writer: WorkerId, tag: u64) -> Result<(), Error> {
        let parked = self.commits.wait_acked(
            writer,
            tag,
            || self.wal.is_poisoned(),
            || self.help_ack(writer, tag),
        )?;
        self.ack_wait_turns.add(parked);
        Ok(())
    }
    #[must_use]
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }
    #[must_use]
    pub fn with_eventual_bound(mut self, bytes: u64) -> Self {
        self.eventual_bound = bytes;
        self
    }
    fn answer(
        &self,
        writer: WorkerId,
        tag: u64,
        entry: &crate::wal::driver::Precommitted,
        took: &[(WorkerId, u64)],
    ) -> Result<bool, Error> {
        for &(on, waited) in took {
            if !self.commits.is_answered(on, waited)? {
                return Ok(false);
            }
        }
        let _bound = bind(u16::try_from(writer.0).map_err(|_| Error::Corrupt)?);
        let mine = self.wal.write_out(true)?.1;
        if mine.upto < entry.upto {
            return Ok(false);
        }
        if mine.upto.saturating_sub(self.wal.mine()?.durable()) > self.eventual_bound {
            return Ok(false);
        }
        for &(other, gsn) in &entry.deps {
            if other as usize == writer.0 {
                continue;
            }
            let _other = bind(u16::try_from(other).map_err(|_| Error::Corrupt)?);
            if self.wal.write_out(true)?.1.gsn < gsn {
                return Ok(false);
            }
        }
        self.refuse_if_stopped()?;
        self.commits.answer(writer, tag)?;
        Ok(true)
    }
    pub fn harden(&self) -> Result<(), Error> {
        for worker in 0..self.workers() {
            let queued = self.wal.queued_all(worker)?;
            for entry in &queued {
                self.help_ack(WorkerId(worker), entry.txn)?;
            }
            if !queued.is_empty() {
                let _ = self.collect(WorkerId(worker), COLLECT_FLOOR.saturating_mul(queued.len()));
            }
        }
        Ok(())
    }
    fn harden_covered(&self, worker: WorkerId) -> Result<(), Error> {
        let _bound = bind(u16::try_from(worker.0).map_err(|_| Error::Corrupt)?);
        let acked = |on: u16, tag: u64| {
            self.commits.is_acked(WorkerId(usize::from(on)), tag).unwrap_or(false)
        };
        while let Some(txn) = self.wal.first_covered(acked)? {
            self.help_ack(worker, txn)?;
        }
        Ok(())
    }
    fn help_ack(&self, writer: WorkerId, tag: u64) -> Result<bool, Error> {
        let Some(entry) = self.wal.queued(writer.0, tag)? else { return Ok(false) };
        let waits = entry.waits.iter().map(|&(on, waited)| (WorkerId(usize::from(on)), waited));
        self.acknowledge(writer, tag, &entry, waits).map_err(|_| {
            self.wal.poison();
            Error::LogPoisoned
        })?;
        self.helped_acks.add(1);
        Ok(true)
    }
    fn acknowledge(
        &self,
        writer: WorkerId,
        tag: u64,
        entry: &crate::wal::driver::Precommitted,
        waits: impl IntoIterator<Item = (WorkerId, u64)>,
    ) -> Result<(), Error> {
        let _bound = bind(u16::try_from(writer.0).map_err(|_| Error::Corrupt)?);
        self.release(writer, entry)?;
        for (on, waited) in waits {
            self.wait_acked(on, waited)?;
        }
        self.refuse_if_stopped()?;
        self.commits.ack(writer, tag)
    }
    #[must_use]
    pub fn ack_parks_and_wakes(&self) -> (u64, u64) {
        self.commits.parks_and_wakes()
    }
    pub fn park_on(&self, scheduler: &Arc<dyn crate::task::Scheduler>) -> bool {
        let barrier = self.wal.barrier().park_on(Arc::clone(scheduler));
        self.commits.park_on(scheduler) && barrier
    }
    #[must_use]
    pub fn ready_dependencies(&self) -> u64 {
        self.ready_deps.get()
    }
    #[must_use]
    pub fn read_only_commits(&self) -> u64 {
        self.read_only_commits.get()
    }
    #[must_use]
    pub fn helped_acks(&self) -> u64 {
        self.helped_acks.get()
    }
    #[must_use]
    pub fn ack_wait_turns(&self) -> u64 {
        self.ack_wait_turns.get()
    }
    #[must_use]
    pub fn undo_held(&self) -> u64 {
        self.undo_held.get()
    }
    #[must_use]
    pub fn release_counts(&self) -> (u64, u64) {
        (self.releases.get(), self.release_yields.get())
    }
    #[must_use]
    pub fn commit_path_counts(&self) -> [u64; 4] {
        [
            self.dep_writes.get(),
            self.idle_writes.get(),
            self.barrier_txns.get(),
            self.wal.chain_slow_paths(),
        ]
    }
    #[must_use]
    pub fn collecting_skips(&self) -> u64 {
        self.collecting_skips.get()
    }
    #[must_use]
    pub fn write_redecisions(&self) -> u64 {
        self.write_redecisions.get()
    }
    pub fn begin(&self, worker: WorkerId) -> Result<Transaction<'_>, Error> {
        if worker.0 >= self.snapshots.workers() {
            return Err(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) });
        }
        if !self.snapshots.beginning(worker) {
            return Err(Error::WorkerBusy { worker: worker.0 });
        }
        let at = match self.clock.snapshot() {
            Ok(at) => at,
            Err(e) => {
                self.snapshots.release(worker);
                return Err(e);
            }
        };
        self.snapshots.publish(worker, at);
        self.wal.begin_txn(worker.0);
        Ok(Transaction {
            db: self,
            worker,
            at,
            wrote: None,
            finished: false,
            doomed: false,
            durability: self.durability,
            took: core::cell::RefCell::new(Dependencies::default()),
            versions_from: self.versions.next_locator(worker)?,
        })
    }
    pub fn get(&self, worker: WorkerId, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        self.lookup(worker, key, <[u8]>::to_vec)
    }
    pub fn lookup<R>(
        &self,
        worker: WorkerId,
        key: &[u8],
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, Error> {
        let txn = self.begin(worker)?;
        let got = txn.lookup(key, f).and_then(|got| txn.settle_all(txn.durability).map(|()| got));
        txn.finish();
        got
    }
    pub fn insert(&self, worker: WorkerId, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let mut txn = self.begin(worker)?;
        match txn.insert(key, value) {
            Ok(()) => txn.commit(),
            Err(e) => {
                txn.abort()?;
                Err(e)
            }
        }
    }
    pub fn remove(&self, worker: WorkerId, key: &[u8]) -> Result<bool, Error> {
        let mut txn = self.begin(worker)?;
        match txn.remove(key) {
            Ok(had) => txn.commit().map(|()| had),
            Err(e) => {
                txn.abort()?;
                Err(e)
            }
        }
    }
}
#[derive(Debug, Default)]
struct UndoLog {
    bytes: Vec<u8>,
    entries: Vec<Undo>,
    tuple: Vec<u8>,
}
#[derive(Debug, Clone, Copy)]
struct Undo {
    at: usize,
    key: usize,
    before: Option<usize>,
    left: Option<u64>,
}
#[derive(Debug, Default)]
struct Removals {
    bytes: std::collections::VecDeque<u8>,
    queued: std::collections::VecDeque<Removal>,
    taking: Vec<u8>,
}
#[derive(Debug, Clone, Copy)]
struct Removal {
    commit_ts: u64,
    tag: u64,
    key: usize,
    len: usize,
}
impl UndoLog {
    fn key(&self, entry: Undo) -> &[u8] {
        &self.bytes[entry.at..entry.at + entry.key]
    }
    fn before(&self, entry: Undo) -> Option<&[u8]> {
        let from = entry.at + entry.key;
        entry.before.map(|len| &self.bytes[from..from + len])
    }
    const KEEP: usize = 1 << 20;
    fn kept_bytes(&self) -> usize {
        self.bytes.capacity()
            + self.entries.capacity() * core::mem::size_of::<Undo>()
            + self.tuple.capacity()
    }
}
std::thread_local! {
    static UNDO_LOG : core::cell::Cell < UndoLog > =
    core::cell::Cell::new(UndoLog::default());
}
#[derive(Debug)]
struct LentUndo(UndoLog);
impl LentUndo {
    fn take() -> Self {
        let mut log = UNDO_LOG.with(core::cell::Cell::take);
        log.bytes.clear();
        log.entries.clear();
        Self(log)
    }
}
impl Drop for LentUndo {
    fn drop(&mut self) {
        let log = core::mem::take(&mut self.0);
        if log.kept_bytes() > UndoLog::KEEP {
            return;
        }
        let _ = UNDO_LOG.try_with(|slot| slot.set(log));
    }
}
#[derive(Debug)]
pub struct Transaction<'a> {
    db: &'a Db,
    worker: WorkerId,
    at: Timestamp,
    wrote: Option<LentUndo>,
    finished: bool,
    doomed: bool,
    durability: Durability,
    took: core::cell::RefCell<Dependencies>,
    versions_from: u64,
}
#[derive(Debug)]
#[must_use = "a transaction is not committed until its acknowledgement is waited for"]
pub struct Ack<'a> {
    db: &'a Db,
    worker: WorkerId,
    tag: u64,
    entry: Option<crate::wal::driver::Precommitted>,
    took: Vec<(WorkerId, u64)>,
    durability: Durability,
    collect: usize,
}
impl Ack<'_> {
    pub fn wait(mut self) -> Result<(), Error> {
        self.finish()
    }
    pub fn wait_durable(mut self) -> Result<(), Error> {
        self.durability = Durability::Durable;
        self.finish()
    }
    fn finish(&mut self) -> Result<(), Error> {
        let Some(entry) = self.entry.take() else {
            for (writer, tag) in core::mem::take(&mut self.took) {
                self.db.confirm_read(self.durability, writer, tag)?;
            }
            return Ok(());
        };
        let took = core::mem::take(&mut self.took);
        if self.durability == Durability::Eventual {
            match self.db.answer(self.worker, self.tag, &entry, &took) {
                Ok(true) => {
                    let _ = self.db.collect(self.worker, self.collect);
                    return Ok(());
                }
                Ok(false) => {}
                Err(_) => {
                    self.db.wal.poison();
                    return Err(Error::InDoubt);
                }
            }
        }
        let outcome = self.db.acknowledge(self.worker, self.tag, &entry, took.iter().copied());
        let outcome = outcome.map_err(|_| {
            self.db.wal.poison();
            Error::InDoubt
        });
        if outcome.is_ok() && self.durability == Durability::Eventual {
            let _ = self.db.harden_covered(self.worker);
        }
        if outcome.is_ok() {
            let _ = self.db.collect(self.worker, self.collect);
        }
        outcome
    }
}
impl Drop for Ack<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
impl<'a> Transaction<'a> {
    #[must_use]
    pub fn at(&self) -> Timestamp {
        self.at
    }
    #[must_use]
    pub fn worker(&self) -> WorkerId {
        self.worker
    }
    pub fn set_durability(&mut self, durability: Durability) {
        self.durability = durability;
    }
    fn as_worker(&self) -> Bound {
        bind(u16::try_from(self.worker.0).unwrap_or(u16::MAX))
    }
    fn reader(&self) -> Reader {
        Reader { worker: self.worker, at: self.at }
    }
    fn settle(&self, seen: Dependencies) {
        self.took.borrow_mut().absorb(seen);
    }
    fn settle_all(&self, durability: Durability) -> Result<(), Error> {
        let mut took = self.took.borrow_mut();
        let unsettled = took.unsettled();
        self.db.ready_deps.add(unsettled.len() as u64);
        for &(writer, tag) in unsettled {
            self.db.confirm_read(durability, writer, tag)?;
        }
        Ok(())
    }
    fn give_back_worker(&self) {
        self.db.wal.end_txn(self.worker.0);
        self.db.snapshots.release(self.worker);
    }
    fn finish(mut self) {
        self.give_back_worker();
        self.finished = true;
    }
    pub fn scan<F>(&self, from: &[u8], visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        self.scan_limit(from, usize::MAX, visit)
    }
    pub fn scan_limit<F>(&self, from: &[u8], limit: usize, visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        let _bound = self.as_worker();
        let mut seen = Dependencies::default();
        let scanned = self.db.scan_at(self.reader(), &mut seen, from, limit, visit);
        self.settle(seen);
        scanned
    }
    pub fn scan_skipping<F>(
        &self,
        from: &[u8],
        skip: usize,
        limit: usize,
        visit: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        let _bound = self.as_worker();
        let mut seen = Dependencies::default();
        let scanned = self.db.scan_from(self.reader(), &mut seen, from, skip, limit, true, visit);
        self.settle(seen);
        scanned
    }
    pub fn scan_keys<F>(
        &self,
        from: &[u8],
        skip: usize,
        limit: usize,
        mut visit: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Step,
    {
        let _bound = self.as_worker();
        let mut seen = Dependencies::default();
        let scanned =
            self.db
                .scan_from(self.reader(), &mut seen, from, skip, limit, false, |key, _| visit(key));
        self.settle(seen);
        scanned
    }
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        self.lookup(key, <[u8]>::to_vec)
    }
    pub fn lookup<R>(&self, key: &[u8], f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>, Error> {
        let _bound = self.as_worker();
        let reader = self.reader();
        let mut seen = Dependencies::default();
        let resolved = self.db.tree.lookup(key, |stored| {
            visible(stored, reader, (&self.db.versions, &self.db.tree), &self.db.commits, &mut seen)
                .map(|value| value.map(|v| f(&v)))
        })?;
        let answer = resolved.unwrap_or(Ok(None));
        self.settle(seen);
        answer
    }
    fn may_write_over(
        db: &Db,
        worker: WorkerId,
        at: Timestamp,
        stored: &[u8],
        deps: &mut Dependencies,
    ) -> Result<bool, Error> {
        let (writer, tx_ts) = match tuple::head_of_stored(stored)? {
            Some(
                tuple::Head::Chained { writer, tx_ts } | tuple::Head::Removed { writer, tx_ts },
            ) => (writer, tx_ts),
            Some(tuple::Head::VisibleForAll) => return Ok(true),
            None => return Err(Error::Corrupt),
        };
        crate::txn::visible::may_see(
            Reader { worker, at },
            WorkerId(usize::from(writer)),
            tx_ts,
            &db.commits,
            deps,
        )
    }
    fn write(
        &mut self,
        key: &[u8],
        payload: Option<&[u8]>,
        absent_only: bool,
    ) -> Result<Option<bool>, Error> {
        if self.doomed {
            return Err(Error::Conflict);
        }
        let _bound = self.as_worker();
        let Self { db, worker, at, wrote, took, doomed, .. } = self;
        let log = &mut wrote.get_or_insert_with(LentUndo::take).0;
        for _ in 0..WRITE_DECISIONS {
            let start = log.bytes.len();
            match Self::write_logged(
                db,
                *worker,
                *at,
                log,
                took.get_mut(),
                key,
                (payload, absent_only),
            ) {
                Ok(Wrote::Written(entry)) => {
                    log.entries.push(entry);
                    let held =
                        log.before(entry).map(Self::holds_value).transpose()?.unwrap_or(false);
                    return Ok(Some(held));
                }
                Ok(Wrote::Present) => {
                    log.bytes.truncate(start);
                    return Ok(None);
                }
                Ok(Wrote::Moved) => {
                    log.bytes.truncate(start);
                    db.write_redecisions.add(1);
                }
                Err(e) => {
                    log.bytes.truncate(start);
                    *doomed |= e == Error::Conflict;
                    return Err(e);
                }
            }
        }
        Err(Error::Livelocked { attempts: WRITE_DECISIONS })
    }
    fn holds_value(stored: &[u8]) -> Result<bool, Error> {
        Ok(!matches!(tuple::head_of_stored(stored)?, Some(tuple::Head::Removed { .. }) | None))
    }
    fn write_logged(
        db: &Db,
        worker: WorkerId,
        at: Timestamp,
        log: &mut UndoLog,
        deps: &mut Dependencies,
        key: &[u8],
        (payload, absent_only): (Option<&[u8]>, bool),
    ) -> Result<Wrote, Error> {
        let UndoLog { bytes, tuple: buf, .. } = log;
        let start = bytes.len();
        bytes.extend_from_slice(key);
        let mut reached = None;
        let found = db.tree.read_stored_into(
            key,
            &mut crate::tree::value::Append::after(bytes),
            &mut reached,
        )?;
        bytes.truncate(start + key.len() + found.unwrap_or(0));
        let entry = Undo { at: start, key: key.len(), before: found, left: None };
        let before = entry.before.map(|len| &bytes[start + key.len()..start + key.len() + len]);
        if let Some(stored) = before
            && !Self::may_write_over(db, worker, at, stored, deps)?
        {
            return Err(Error::Conflict);
        }
        if absent_only && before.map(Self::holds_value).transpose()?.unwrap_or(false) {
            return Ok(Wrote::Present);
        }
        let previous = match before {
            None => tuple::NO_PREVIOUS,
            Some(stored) => db.versions.put(worker, stored)?,
        };
        let written = Self::write_displaced(
            db,
            worker,
            at,
            (entry, previous),
            (key, before, payload),
            (buf, reached.as_ref()),
        );
        if !matches!(written, Ok(Wrote::Written(_))) && before.is_some() {
            db.versions.take_back(worker, previous)?;
        }
        written
    }
    fn write_displaced(
        db: &Db,
        worker: WorkerId,
        at: Timestamp,
        (mut entry, previous): (Undo, u64),
        (key, before, payload): (&[u8], Option<&[u8]>, Option<&[u8]>),
        (buf, from): (&mut Vec<u8>, Option<&crate::tree::Leaf<'_>>),
    ) -> Result<Wrote, Error> {
        let header = tuple::Version {
            writer: u16::try_from(worker.0).unwrap_or(u16::MAX),
            tx_ts: at.get(),
            previous,
        };
        if payload.is_none() {
            entry.left = Some(previous);
        }
        match payload {
            Some(value) => tuple::encode_chained(header, value, buf),
            None => tuple::encode_removed(header, buf),
        }
        let txn = at.get();
        let head = buf.get(..tuple::CHAINED_HEADER_BYTES).ok_or(Error::Corrupt)?;
        let part = crate::wal::record::payload::UndoPart { key, head, before };
        let mut reserved = db.wal.reserve_change(&part);
        for _ in 0..db.room_attempts() {
            if !matches!(reserved, Err(Error::LogFull { .. })) || db.catalog.is_none() {
                break;
            }
            db.make_room();
            reserved = db.wal.reserve_change(&part);
        }
        reserved?;
        let undo = crate::tree::TxnUndo { txn, part: &part, from };
        if !db.tree.compare_stored_and_insert_logged(key, before, buf, undo)? {
            return Ok(Wrote::Moved);
        }
        if let Some(stored) = before {
            let gsn = db.wal.slots_ref().get(worker.0).map_or(u64::MAX, |s| s.clock.now());
            db.tree.transferred(stored, gsn);
        }
        Ok(Wrote::Written(entry))
    }
    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.write(key, Some(value), false).map(|_| ())
    }
    pub fn insert_new(&mut self, key: &[u8], value: &[u8]) -> Result<bool, Error> {
        self.write(key, Some(value), true).map(|wrote| wrote.is_some())
    }
    pub fn remove(&mut self, key: &[u8]) -> Result<bool, Error> {
        self.write(key, None, false).map(|held| held == Some(true))
    }
    pub fn commit(self) -> Result<(), Error> {
        self.precommit()?.wait()
    }
    pub fn precommit(mut self) -> Result<Ack<'a>, Error> {
        let _bound = self.as_worker();
        self.db.refuse_if_stopped()?;
        if self.doomed {
            return Err(Error::Conflict);
        }
        if self.wrote.is_none() {
            return Ok(self.commit_read_only());
        }
        let versions_upto = self.db.versions.next_locator(self.worker)?;
        let named: Vec<(u16, u64, u64)> = self
            .took
            .get_mut()
            .taken()
            .iter()
            .filter(|&&(writer, tag)| !self.db.commits.is_acked(writer, tag).unwrap_or(false))
            .filter_map(|&(writer, tag)| {
                let upto = self.db.wal.queued_upto(writer.0, tag)?;
                Some((u16::try_from(writer.0).ok()?, tag, upto))
            })
            .collect();
        let waits: Vec<(u16, u64)> = self
            .took
            .get_mut()
            .taken()
            .iter()
            .filter(|&&(writer, tag)| !self.db.commits.is_acked(writer, tag).unwrap_or(false))
            .filter_map(|&(writer, tag)| Some((u16::try_from(writer.0).ok()?, tag)))
            .collect();
        let entry = self.db.wal.precommit_waiting(self.at.get(), &named, waits)?;
        let settled = self.took.get_mut().unsettled().len();
        self.db.ready_deps.add(settled as u64);
        let commit_ts =
            match self.db.commits.commit(self.worker, self.at, &self.db.clock, versions_upto) {
                Ok(ts) => ts,
                Err(cause) => return Err(self.in_doubt(cause)),
            };
        self.finished = true;
        self.queue_removals(commit_ts.get());
        let took = std::mem::take(self.took.get_mut()).into_taken();
        self.give_back_worker();
        if commit_ts.get() % self.db.workers().max(1) as u64 == 0 {
            let _ = self.db.refresh_visible_to_all(self.worker);
        }
        if self.db.catalog.is_some() {
            self.db.pace(self.worker.0);
        }
        let wrote = versions_upto.saturating_sub(self.versions_from);
        let collect =
            usize::try_from(wrote.saturating_mul(2)).unwrap_or(usize::MAX).max(COLLECT_FLOOR);
        Ok(Ack {
            db: self.db,
            worker: self.worker,
            tag: self.at.get(),
            entry: Some(entry),
            took,
            durability: self.durability,
            collect,
        })
    }
    fn commit_read_only(self) -> Ack<'a> {
        let owed = self.took.borrow_mut().unsettled().to_vec();
        self.db.ready_deps.add(owed.len() as u64);
        let (db, worker, tag, durability) = (self.db, self.worker, self.at.get(), self.durability);
        let nth = db.snapshots.read_only_commit(worker);
        self.finish();
        db.read_only_commits.add(1);
        if nth.is_multiple_of(db.workers().max(1) as u64) {
            let _ = db.refresh_visible_to_all(worker);
        }
        if db.catalog.is_some() {
            db.pace(worker.0);
        }
        let _ = db.collect(worker, COLLECT_FLOOR);
        Ack { db, worker, tag, entry: None, took: owed, durability, collect: 0 }
    }
    fn queue_removals(&mut self, commit_ts: u64) {
        let Some(LentUndo(log)) = self.wrote.as_mut() else { return };
        let UndoLog { bytes, entries, tuple: buf } = log;
        let mut queue = None;
        for &entry in entries.iter() {
            let Some(previous) = entry.left else { continue };
            let header = tuple::Version {
                writer: u16::try_from(self.worker.0).unwrap_or(u16::MAX),
                tx_ts: self.at.get(),
                previous,
            };
            tuple::encode_removed(header, buf);
            let key = &bytes[entry.at..entry.at + entry.key];
            let queue = queue.get_or_insert_with(|| {
                self.db.removals[self.worker.0]
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            queue.bytes.extend(key);
            queue.bytes.extend(buf.iter());
            queue.queued.push_back(Removal {
                commit_ts,
                tag: self.at.get(),
                key: key.len(),
                len: key.len() + buf.len(),
            });
        }
    }
    fn in_doubt(&self, _cause: Error) -> Error {
        self.db.wal.poison();
        Error::InDoubt
    }
    pub fn abort(&mut self) -> Result<(), Error> {
        let _bound = self.as_worker();
        let outcome = self.roll_back();
        let confirmed = self.settle_all(Durability::Durable);
        self.give_back_worker();
        self.finished = true;
        outcome.and(confirmed)
    }
    fn roll_back(&mut self) -> Result<(), Error> {
        if let Some(LentUndo(log)) = self.wrote.as_mut() {
            let outcome = Self::undo_entries(self.db, log);
            let wrote = !log.entries.is_empty();
            log.entries.clear();
            log.bytes.clear();
            if wrote {
                self.db.clock.take()?;
            }
            outcome?;
        }
        self.db.wal.aborted(self.at.get())?;
        Ok(())
    }
    fn undo_entries(db: &Db, log: &UndoLog) -> Result<(), Error> {
        for &entry in log.entries.iter().rev() {
            let key = log.key(entry);
            let undone = match log.before(entry) {
                Some(bytes) => db.tree.value_of_stored(bytes).and_then(|v| db.tree.insert(key, &v)),
                None => db.tree.remove(key).map(|_| ()),
            };
            if let Err(e) = undone {
                db.wal.poison();
                return Err(e);
            }
        }
        Ok(())
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _bound = self.as_worker();
            let _ = self.roll_back();
            let _ = self.settle_all(Durability::Durable);
            self.give_back_worker();
        }
    }
}
impl Db {
    pub(crate) fn scan_at<F>(
        &self,
        reader: Reader,
        deps: &mut Dependencies,
        from: &[u8],
        limit: usize,
        visit: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        self.scan_from(reader, deps, from, 0, limit, true, visit)
    }
    pub(crate) fn scan_from<F>(
        &self,
        reader: Reader,
        deps: &mut Dependencies,
        from: &[u8],
        skip: usize,
        limit: usize,
        values: bool,
        mut visit: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&[u8], &[u8]) -> Step,
    {
        if !self.snapshots.holds(reader.worker, reader.at) {
            return Err(Error::SnapshotNotHeld { worker: reader.worker.0 });
        }
        let (mut skip, mut left) = (skip, limit);
        if left == 0 {
            return Ok(());
        }
        let mut lent = LentScan::take();
        let ScanBuffers { from: resume, next } = &mut lent.0;
        resume.clear();
        resume.extend_from_slice(from);
        let mut decided = Tally(0, &self.scan_rows_decided);
        let mut resolved = Tally(0, &self.scan_rows_resolved);
        let mut walks: u32 = 0;
        loop {
            let mut ended = Ended::Range;
            let mut refused = None;
            next.clear();
            let heads = skip > 0 || !values;
            let mut decide = |key: &[u8], bytes: &[u8], whole: bool, payload: Option<&[u8]>| {
                let seen = match head_visible(bytes, whole, reader, &self.commits, deps) {
                    Ok(seen) => seen,
                    Err(e) => {
                        refused = Some(e);
                        return Step::Stop;
                    }
                };
                match seen {
                    Seen::Removed => return Step::Continue,
                    Seen::Behind => {
                        next.extend_from_slice(key);
                        ended = Ended::Behind;
                        return Step::Stop;
                    }
                    Seen::Value => decided.0 += 1,
                }
                if skip > 0 {
                    skip -= 1;
                    if skip == 0 && values {
                        next.extend_from_slice(key);
                        ended = Ended::Passed;
                        return Step::Stop;
                    }
                    return Step::Continue;
                }
                let value = match payload.map(tuple::decode).transpose() {
                    Ok(stored) => stored.and_then(tuple::Stored::payload).unwrap_or_default(),
                    Err(e) => {
                        refused = Some(e);
                        return Step::Stop;
                    }
                };
                left -= 1;
                if visit(key, value) == Step::Stop || left == 0 {
                    ended = Ended::Done;
                    return Step::Stop;
                }
                Step::Continue
            };
            if heads {
                let most = tuple::CHAINED_HEADER_BYTES + 1;
                self.tree.scan_local(resume, most, |key, local| match local {
                    Local::Inline(b) => decide(key, b, b.len() < most, None),
                    Local::Separated(b) => decide(key, b, false, None),
                })?;
            } else {
                self.tree.scan(resume, |key, stored| decide(key, stored, true, Some(stored)))?;
            }
            if let Some(e) = refused {
                return Err(e);
            }
            match ended {
                Ended::Range | Ended::Done => return Ok(()),
                Ended::Passed => {}
                Ended::Behind => {
                    resolved.0 += 1;
                    let mut stop = false;
                    let found = self.tree.lookup(next, |stored| -> Result<(), Error> {
                        let Some(value) = visible(
                            stored,
                            reader,
                            (&self.versions, &self.tree),
                            &self.commits,
                            deps,
                        )?
                        else {
                            return Ok(());
                        };
                        if skip > 0 {
                            skip -= 1;
                        } else {
                            left -= 1;
                            stop = visit(next, if values { &value } else { &[] }) == Step::Stop
                                || left == 0;
                        }
                        Ok(())
                    })?;
                    found.transpose()?;
                    if stop {
                        return Ok(());
                    }
                }
            }
            next.push(0);
            walks = walks.saturating_add(1);
            if next.as_slice() <= resume.as_slice() {
                return Err(Error::Livelocked { attempts: walks });
            }
            core::mem::swap(resume, next);
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Range,
    Done,
    Passed,
    Behind,
}
struct Tally<'t>(u64, &'t crate::stats::Stat);
impl Drop for Tally<'_> {
    fn drop(&mut self) {
        if self.0 > 0 {
            self.1.add(self.0);
        }
    }
}
impl Db {
    pub fn collect(&self, worker: WorkerId, limit: usize) -> Result<usize, Error> {
        let floor = self.clock.snapshot()?;
        let bound = match self.snapshots.watermark(worker) {
            Watermark::Beginning => return Ok(0),
            Watermark::Empty => collect_bound_at(None, floor)?,
            Watermark::Oldest(at) => collect_bound_at(Some(at), floor)?,
        };
        let mut collecting = None;
        self.collect_removals(worker, bound, limit, &mut collecting)?;
        let upto = self.commits.collectable_upto(worker, bound)?;
        let Some(upto) = upto else { return Ok(0) };
        let purged = self.versions.purge_with(worker, upto, limit, |stored| {
            self.tree.retire_stored(stored, worker.0);
        })?;
        let _ = self.commits.compact(worker, bound)?;
        Ok(purged)
    }
    fn collect_removals<'d>(
        &'d self,
        worker: WorkerId,
        bound: Timestamp,
        limit: usize,
        collecting: &mut Option<crate::wal::collecting::Guard<'d>>,
    ) -> Result<usize, Error> {
        let Some(queue) = self.removals.get(worker.0) else { return Ok(0) };
        let lock = || queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _bound = bind(u16::try_from(worker.0).unwrap_or(u16::MAX));
        let mut removed = 0;
        for _ in 0..limit {
            let (removal, taking) = {
                let mut q = lock();
                let due = match q.queued.front() {
                    Some(r) => r.commit_ts < bound.get() && self.commits.is_acked(worker, r.tag)?,
                    None => false,
                };
                let Some(removal) = (if due { q.queued.pop_front() } else { None }) else {
                    break;
                };
                let mut taking = core::mem::take(&mut q.taking);
                taking.clear();
                taking.extend(q.bytes.drain(..removal.len));
                (removal, taking)
            };
            if collecting.is_none() {
                *collecting = self.wal.collecting(worker.0);
            }
            let (key, stored) = taking.split_at(removal.key);
            let head = stored.get(..tuple::CHAINED_HEADER_BYTES).ok_or(Error::Corrupt)?;
            let part = crate::wal::record::payload::UndoPart { key, head, before: Some(stored) };
            let outcome = self
                .wal
                .reserve_change(&part)
                .and_then(|()| self.tree.compare_and_remove(key, stored));
            let mut q = lock();
            match outcome {
                Ok(taken) => {
                    removed += usize::from(taken);
                    q.taking = taking;
                }
                Err(e) => {
                    for &b in taking.iter().rev() {
                        q.bytes.push_front(b);
                    }
                    q.queued.push_front(removal);
                    q.taking = taking;
                    return if matches!(e, Error::LogFull { .. }) { Ok(removed) } else { Err(e) };
                }
            }
        }
        Ok(removed)
    }
    pub fn refresh_visible_to_all(&self, asking: WorkerId) -> Result<bool, Error> {
        let floor = self.clock.snapshot()?;
        let oldest = match self.snapshots.watermark(asking) {
            Watermark::Beginning => return Ok(false),
            Watermark::Empty => collect_bound_at(None, floor)?,
            Watermark::Oldest(at) => collect_bound_at(Some(at), floor)?,
        };
        let mut moved = false;
        for index in 0..self.workers() {
            let worker = WorkerId(index);
            if let Some(cut) = self.commits.lcb(worker, oldest)? {
                moved |= self.commits.publish_all_lwm(worker, cut.saturating_add(1));
            }
        }
        Ok(moved)
    }
    pub fn collect_all(&self, limit: usize) -> Result<usize, Error> {
        let _ = self.refresh_visible_to_all(WorkerId(0))?;
        let mut removed = 0;
        for index in 0..self.workers() {
            removed += self.collect(WorkerId(index), limit)?;
        }
        Ok(removed)
    }
}
