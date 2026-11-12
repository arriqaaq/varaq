use crate::tree::scan::Step;
use crate::txn::WorkerId;
use crate::{Error, Tree};
use core::sync::atomic::{AtomicU64, Ordering};
const KEY_BYTES: usize = 8;
const MIN_FRAMES_PER_WORKER: usize = 64;
std::thread_local! {
    static DOOMED : core::cell::RefCell < Vec < Doomed >> = const {
    core::cell::RefCell::new(Vec::new()) };
}
#[derive(Clone, Copy)]
struct Doomed {
    key: [u8; KEY_BYTES],
    token: ([u8; crate::node::blob::TOKEN_MOST], u8),
}
fn key(locator: u64) -> [u8; KEY_BYTES] {
    locator.to_be_bytes()
}
#[derive(Debug)]
pub struct Versions {
    trees: Box<[Tree]>,
    next: Box<[crate::sync::Padded<AtomicU64>]>,
    purged: Box<[crate::sync::Padded<AtomicU64>]>,
}
impl Versions {
    pub fn with_capacity(workers: usize, capacity: usize) -> Result<Self, Error> {
        let per = Self::frames_per_worker(workers, capacity);
        let trees =
            (0..workers).map(|_| Tree::with_capacity(per)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self::build(workers, trees))
    }
    pub fn with_store(
        workers: usize,
        capacity: usize,
        store: Box<dyn crate::store::PageStore>,
    ) -> Result<Self, Error> {
        let per = Self::frames_per_worker(workers, capacity);
        let held = (1..=store.len() as u64)
            .map(|id| crate::PageId::new(id).map(|page| (page, 0)))
            .collect::<Result<Vec<_>, _>>()?;
        let reissuable = std::sync::Arc::new(crate::sync::Mutex::new(held));
        let store: std::sync::Arc<dyn crate::store::PageStore> = std::sync::Arc::from(store);
        let trees = (0..workers)
            .map(|_| {
                Tree::with_store_reissuing(
                    per,
                    Box::new(std::sync::Arc::clone(&store)),
                    std::sync::Arc::clone(&reissuable),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::build(workers, trees))
    }
    fn frames_per_worker(workers: usize, capacity: usize) -> usize {
        (capacity / workers.max(1)).max(MIN_FRAMES_PER_WORKER)
    }
    fn build(workers: usize, trees: Vec<Tree>) -> Self {
        assert!(workers > 0, "a version store needs at least one worker");
        Self {
            trees: trees.into_boxed_slice(),
            next: (0..workers)
                .map(|_| crate::sync::Padded(AtomicU64::new(1)))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            purged: (0..workers)
                .map(|_| crate::sync::Padded(AtomicU64::new(1)))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }
    #[must_use]
    pub fn workers(&self) -> usize {
        self.next.len()
    }
    pub fn put(&self, writer: WorkerId, version: &[u8]) -> Result<u64, Error> {
        let counter = self
            .next
            .get(writer.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(writer.0).unwrap_or(u32::MAX) })?;
        let locator = counter.fetch_add(1, Ordering::Relaxed);
        self.tree_of(writer)?.insert(&key(locator), version)?;
        Ok(locator)
    }
    pub fn take_back(&self, writer: WorkerId, locator: u64) -> Result<(), Error> {
        self.tree_of(writer)?.remove(&key(locator)).map(|_| ())
    }
    pub fn get(&self, writer: WorkerId, locator: u64) -> Result<Option<Vec<u8>>, Error> {
        self.tree_of(writer)?.get(&key(locator))
    }
    fn tree_of(&self, writer: WorkerId) -> Result<&Tree, Error> {
        self.trees
            .get(writer.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(writer.0).unwrap_or(u32::MAX) })
    }
    pub fn purge(&self, writer: WorkerId, upto: u64, limit: usize) -> Result<usize, Error> {
        self.purge_with(writer, upto, limit, |_| {})
    }
    pub fn purge_with(
        &self,
        writer: WorkerId,
        upto: u64,
        limit: usize,
        mut removed: impl FnMut(&[u8]),
    ) -> Result<usize, Error> {
        if limit == 0 {
            return Ok(0);
        }
        let purged = self
            .purged
            .get(writer.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(writer.0).unwrap_or(u32::MAX) })?;
        let below = purged.load(Ordering::Acquire);
        if upto <= below {
            return Ok(0);
        }
        let tree = self.tree_of(writer)?;
        let from = key(below);
        let mut doomed = DOOMED.with(|d| core::mem::take(&mut *d.borrow_mut()));
        doomed.clear();
        tree.scan_local(&from, crate::node::blob::TOKEN_MOST, |k, local| {
            if k.len() != KEY_BYTES {
                return Step::Stop;
            }
            let mut loc = [0u8; 8];
            loc.copy_from_slice(k);
            if u64::from_be_bytes(loc) >= upto {
                return Step::Stop;
            }
            let mut entry = Doomed { key: loc, token: ([0; crate::node::blob::TOKEN_MOST], 0) };
            if let crate::tree::scan::Local::Inline(v) = local
                && crate::node::blob::of_token(v).is_some()
                && let Ok(n) = u8::try_from(v.len())
                && let Some(into) = entry.token.0.get_mut(..v.len())
            {
                into.copy_from_slice(v);
                entry.token.1 = n;
            }
            doomed.push(entry);
            if doomed.len() >= limit { Step::Stop } else { Step::Continue }
        })?;
        let mut gone = 0;
        for d in &doomed {
            if tree.remove(&d.key[..])? {
                gone += 1;
                if d.token.1 > 0 {
                    removed(&d.token.0[..usize::from(d.token.1)]);
                }
            }
        }
        let reached = if doomed.len() < limit {
            upto
        } else {
            doomed.last().map_or(below, |d| u64::from_be_bytes(d.key).saturating_add(1))
        };
        purged.fetch_max(reached, Ordering::AcqRel);
        DOOMED.with(|d| *d.borrow_mut() = doomed);
        Ok(gone)
    }
    pub fn next_locator(&self, worker: WorkerId) -> Result<u64, Error> {
        let counter = self
            .next
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        Ok(counter.load(Ordering::Acquire))
    }
    pub fn set_separate_above(&mut self, line: usize) {
        for tree in &mut self.trees {
            tree.set_separate_above(line);
        }
    }
    pub(crate) fn of_store(
        workers: usize,
        capacity: usize,
        store: Option<Box<dyn crate::store::PageStore>>,
        wal: &std::sync::Arc<crate::wal::worker::Wal>,
    ) -> Result<Self, Error> {
        let versions = match store {
            Some(store) => Self::with_store(workers, capacity, store)?,
            None => Self::with_capacity(workers, capacity)?,
        };
        Ok(versions.stopping_with(wal))
    }
    fn stopping_with(mut self, wal: &std::sync::Arc<crate::wal::worker::Wal>) -> Self {
        for tree in &mut self.trees {
            tree.stop_with(wal);
        }
        self
    }
    #[must_use]
    pub fn trees(&self) -> &[Tree] {
        &self.trees
    }
}
