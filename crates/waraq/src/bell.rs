use crate::Error;
use crate::stats::Stat;
use crate::sync::{Condvar, Mutex, MutexGuard};
use crate::task::{Scheduler, Ticket};
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
#[derive(Debug, Default)]
struct Lot {
    parked: Vec<Ticket>,
    sleepers: u32,
}
pub struct Bell {
    lot: Mutex<Lot>,
    rung: Condvar,
    scheduler: OnceLock<Arc<dyn Scheduler>>,
    waiting: AtomicU64,
    parks: Stat,
    wakes: Stat,
}
impl core::fmt::Debug for Bell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bell")
            .field("waiters", if self.scheduler.get().is_some() { &"parked" } else { &"blocked" })
            .finish_non_exhaustive()
    }
}
impl Default for Bell {
    fn default() -> Self {
        Self::new()
    }
}
impl Bell {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lot: Mutex::new(Lot::default()),
            rung: Condvar::new(),
            scheduler: OnceLock::new(),
            waiting: AtomicU64::new(0),
            parks: Stat::default(),
            wakes: Stat::default(),
        }
    }
    pub fn park_on(&self, scheduler: Arc<dyn Scheduler>) -> bool {
        self.scheduler.set(scheduler).is_ok()
    }
    #[must_use]
    pub fn scheduled(&self) -> bool {
        self.scheduler.get().is_some()
    }
    fn lot(&self) -> Result<MutexGuard<'_, Lot>, Error> {
        self.lot.lock().map_err(|_| Error::Corrupt)
    }
    pub fn park(&self, moved: impl Fn() -> bool, armed: &mut Option<Ticket>) -> Result<(), Error> {
        let Some(scheduler) = self.scheduler.get() else {
            let _counted = Counted::new(&self.waiting);
            let mut lot = self.lot()?;
            if !moved() {
                lot.sleepers += 1;
                self.parks.add(1);
                lot = self.rung.wait(lot).map_err(|_| Error::Corrupt)?;
                lot.sleepers -= 1;
                self.wakes.add(1);
            }
            return Ok(());
        };
        let Some(ticket) = armed.take() else {
            *armed = Some(scheduler.arm());
            return Ok(());
        };
        let counted = Counted::new(&self.waiting);
        let mut lot = self.lot()?;
        if moved() {
            *armed = Some(ticket);
            return Ok(());
        }
        lot.parked.push(ticket);
        drop(lot);
        self.parks.add(1);
        scheduler.wait(ticket);
        drop(counted);
        Ok(())
    }
    pub fn ring(&self) -> Result<(), Error> {
        let (parked, sleepers) = {
            let mut lot = self.lot()?;
            (core::mem::take(&mut lot.parked), lot.sleepers)
        };
        if sleepers > 0 {
            self.rung.notify_all();
        }
        if let Some(scheduler) = self.scheduler.get() {
            self.wakes.add(parked.len() as u64);
            for ticket in parked {
                scheduler.wake(ticket);
            }
        }
        Ok(())
    }
    pub fn ring_if_waiting(&self) -> Result<(), Error> {
        if self.waiting.fetch_add(0, Ordering::AcqRel) == 0 {
            return Ok(());
        }
        self.ring()
    }
    pub fn hand_back(&self, armed: Option<Ticket>) {
        if let (Some(ticket), Some(scheduler)) = (armed, self.scheduler.get()) {
            scheduler.wake(ticket);
            scheduler.wait(ticket);
        }
    }
    #[must_use]
    pub fn parks_and_wakes(&self) -> (u64, u64) {
        (self.parks.get(), self.wakes.get())
    }
}
struct Counted<'a>(&'a AtomicU64);
impl<'a> Counted<'a> {
    fn new(waiting: &'a AtomicU64) -> Self {
        waiting.fetch_add(1, Ordering::AcqRel);
        Self(waiting)
    }
}
impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
