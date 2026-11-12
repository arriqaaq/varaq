use crate::Error;
use crate::bell::Bell;
use crate::sync::RwLock;
use crate::task::Ticket;
use crate::txn::clock::Clock;
use crate::txn::{Timestamp, WorkerId};
use core::sync::atomic::{AtomicU64, Ordering};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    commit_ts: u64,
    start_ts: u64,
    versions_upto: u64,
    acked: bool,
    answered: bool,
}
fn cut(entries: &[Entry], snapshot: u64) -> Option<u64> {
    let at = entries.partition_point(|e| e.commit_ts < snapshot);
    if at == 0 { None } else { Some(entries[at - 1].start_ts) }
}
#[derive(Debug, Default)]
struct Worker {
    entries: Vec<Entry>,
}
#[derive(Debug)]
pub struct Commits {
    workers: Box<[RwLock<Worker>]>,
    all_lwm: Box<[AtomicU64]>,
    acked_upto: Box<[AtomicU64]>,
    bells: Box<[Bell]>,
}
impl Commits {
    #[must_use]
    pub fn new(workers: usize) -> Self {
        assert!(workers > 0, "a commit registry needs at least one worker");
        let slots = (0..workers).map(|_| RwLock::new(Worker::default())).collect::<Vec<_>>();
        Self {
            workers: slots.into_boxed_slice(),
            all_lwm: (0..workers).map(|_| AtomicU64::new(0)).collect::<Vec<_>>().into_boxed_slice(),
            acked_upto: (0..workers)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            bells: (0..workers).map(|_| Bell::new()).collect::<Vec<_>>().into_boxed_slice(),
        }
    }
    pub fn park_on(&self, scheduler: &std::sync::Arc<dyn crate::task::Scheduler>) -> bool {
        let mut all = true;
        for bell in &self.bells {
            all &= bell.park_on(std::sync::Arc::clone(scheduler));
        }
        all
    }
    pub fn wait_acked(
        &self,
        worker: WorkerId,
        tag: u64,
        stopped: impl Fn() -> bool,
        mut help: impl FnMut() -> Result<bool, Error>,
    ) -> Result<u64, Error> {
        let bell = self
            .bells
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let mut armed: Option<Ticket> = None;
        let mut parked = 0u64;
        let outcome = loop {
            match self.is_acked(worker, tag) {
                Ok(true) => break Ok(parked),
                Ok(false) => {}
                Err(e) => break Err(e),
            }
            if stopped() {
                break Err(Error::LogPoisoned);
            }
            match help() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => break Err(e),
            }
            let moved = || self.is_acked(worker, tag).unwrap_or(true) || stopped();
            if let Err(e) = bell.park(moved, &mut armed) {
                break Err(e);
            }
            parked += 1;
        };
        bell.hand_back(armed);
        outcome
    }
    #[must_use]
    pub fn parks_and_wakes(&self) -> (u64, u64) {
        self.bells.iter().map(Bell::parks_and_wakes).fold((0, 0), |(p, w), (a, b)| (p + a, w + b))
    }
    pub fn ring_all(&self) -> Result<(), Error> {
        self.bells.iter().try_for_each(Bell::ring)
    }
    #[must_use = "whether the mark moved is what a caller reports as progress"]
    pub fn publish_all_lwm(&self, worker: WorkerId, lwm: u64) -> bool {
        self.all_lwm.get(worker.0).is_some_and(|m| m.fetch_max(lwm, Ordering::Release) < lwm)
    }
    #[must_use]
    pub fn all_lwm(&self, worker: WorkerId) -> u64 {
        self.all_lwm.get(worker.0).map_or(0, |m| m.load(Ordering::Acquire))
    }
    #[must_use]
    pub fn workers(&self) -> usize {
        self.workers.len()
    }
    pub fn commit(
        &self,
        worker: WorkerId,
        start_ts: Timestamp,
        clock: &Clock,
        versions_upto: u64,
    ) -> Result<Timestamp, Error> {
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let mut w = slot.write().map_err(|_| Error::Corrupt)?;
        if w.entries.last().is_some_and(|last| start_ts.get() <= last.start_ts) {
            return Err(Error::Corrupt);
        }
        let commit_ts = clock.take()?;
        if let Some(last) = w.entries.last()
            && commit_ts.get() <= last.commit_ts
        {
            return Err(Error::Corrupt);
        }
        w.entries.push(Entry {
            commit_ts: commit_ts.get(),
            start_ts: start_ts.get(),
            versions_upto,
            acked: false,
            answered: false,
        });
        Ok(commit_ts)
    }
    pub fn is_acked(&self, worker: WorkerId, tag: u64) -> Result<bool, Error> {
        if self.acked_upto.get(worker.0).is_some_and(|m| tag <= m.load(Ordering::Acquire)) {
            return Ok(true);
        }
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let r = slot.read().map_err(|_| Error::Corrupt)?;
        let at = r.entries.partition_point(|e| e.start_ts < tag);
        Ok(r.entries.get(at).filter(|e| e.start_ts == tag).is_none_or(|e| e.acked))
    }
    pub fn is_answered(&self, worker: WorkerId, tag: u64) -> Result<bool, Error> {
        if self.is_acked(worker, tag)? {
            return Ok(true);
        }
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let r = slot.read().map_err(|_| Error::Corrupt)?;
        let at = r.entries.partition_point(|e| e.start_ts < tag);
        Ok(r.entries.get(at).filter(|e| e.start_ts == tag).is_some_and(|e| e.answered || e.acked))
    }
    pub fn answer(&self, worker: WorkerId, tag: u64) -> Result<(), Error> {
        if self.acked_upto.get(worker.0).is_some_and(|m| tag <= m.load(Ordering::Acquire)) {
            return Ok(());
        }
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let mut w = slot.write().map_err(|_| Error::Corrupt)?;
        if self.acked_upto.get(worker.0).is_some_and(|m| tag <= m.load(Ordering::Acquire)) {
            return Ok(());
        }
        let at = w.entries.partition_point(|e| e.start_ts < tag);
        let entry = w.entries.get_mut(at).filter(|e| e.start_ts == tag).ok_or(Error::Corrupt)?;
        entry.answered = true;
        Ok(())
    }
    pub fn ack(&self, worker: WorkerId, tag: u64) -> Result<(), Error> {
        if self.acked_upto.get(worker.0).is_some_and(|m| tag <= m.load(Ordering::Acquire)) {
            return Ok(());
        }
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let mut w = slot.write().map_err(|_| Error::Corrupt)?;
        if self.acked_upto.get(worker.0).is_some_and(|m| tag <= m.load(Ordering::Acquire)) {
            return Ok(());
        }
        let at = w.entries.partition_point(|e| e.start_ts < tag);
        let entry = w.entries.get_mut(at).filter(|e| e.start_ts == tag).ok_or(Error::Corrupt)?;
        entry.acked = true;
        let prefix = self.acked_upto.get(worker.0).ok_or(Error::Corrupt)?;
        let from = prefix.load(Ordering::Acquire);
        let first = w.entries.partition_point(|e| e.start_ts <= from);
        let upto =
            w.entries[first..].iter().take_while(|e| e.acked).last().map_or(from, |e| e.start_ts);
        prefix.fetch_max(upto, Ordering::Release);
        drop(w);
        self.bells.get(worker.0).ok_or(Error::Corrupt)?.ring_if_waiting()
    }
    pub fn lcb(&self, worker: WorkerId, snapshot: Timestamp) -> Result<Option<u64>, Error> {
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let r = slot.read().map_err(|_| Error::Corrupt)?;
        Ok(cut(&r.entries, snapshot.get()))
    }
    pub fn visible(&self, worker: WorkerId, tag: u64, snapshot: Timestamp) -> Result<bool, Error> {
        if tag < self.all_lwm.get(worker.0).map_or(0, |m| m.load(Ordering::Acquire)) {
            return Ok(true);
        }
        Ok(match self.lcb(worker, snapshot)? {
            None => false,
            Some(cut) => tag <= cut,
        })
    }
    pub fn collectable_upto(
        &self,
        worker: WorkerId,
        watermark: Timestamp,
    ) -> Result<Option<u64>, Error> {
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let r = slot.read().map_err(|_| Error::Corrupt)?;
        let found = r.entries.partition_point(|e| e.commit_ts < watermark.get());
        let prefix = self.acked_upto.get(worker.0).map_or(0, |m| m.load(Ordering::Acquire));
        let found = found.min(r.entries.partition_point(|e| e.start_ts <= prefix));
        Ok(if found == 0 { None } else { Some(r.entries[found - 1].versions_upto) })
    }
    pub fn collectable_upto_all(&self, worker: WorkerId) -> Result<Option<u64>, Error> {
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let r = slot.read().map_err(|_| Error::Corrupt)?;
        Ok(r.entries.last().map(|e| e.versions_upto))
    }
    pub fn len(&self, worker: WorkerId) -> Result<usize, Error> {
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        Ok(slot.read().map_err(|_| Error::Corrupt)?.entries.len())
    }
    pub fn compact(&self, worker: WorkerId, bound: Timestamp) -> Result<usize, Error> {
        let slot = self
            .workers
            .get(worker.0)
            .ok_or(Error::NoSuchFrame { index: u32::try_from(worker.0).unwrap_or(u32::MAX) })?;
        let mut w = slot.write().map_err(|_| Error::Corrupt)?;
        let cut = w.entries.partition_point(|e| e.commit_ts < bound.get());
        let unacked = w.entries.iter().position(|e| !e.acked).unwrap_or(w.entries.len());
        let drop = cut.saturating_sub(1).min(unacked);
        w.entries.drain(..drop);
        Ok(drop)
    }
}
