use crate::Error;
use crate::bell::Bell;
use crate::stats::Stat;
use crate::store::PageStore;
use crate::sync::Mutex;
use crate::task::{Scheduler, Ticket};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
pub const SPIN_TURNS: u32 = 1024;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    started: u64,
    finished: u64,
    busy: bool,
    failed: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Released,
    Refused,
    Lead(u64),
    Wait,
}
impl State {
    #[must_use]
    pub fn join(&self) -> u64 {
        self.started + 1
    }
    #[must_use]
    pub fn step(&self, need: u64) -> Step {
        if self.failed {
            return Step::Refused;
        }
        if self.finished >= need {
            return Step::Released;
        }
        if self.busy {
            return Step::Wait;
        }
        Step::Lead(self.started + 1)
    }
    #[must_use]
    pub fn begun(self, n: u64) -> Self {
        Self { started: n, busy: true, ..self }
    }
    pub fn complete(&mut self, n: u64, ok: bool) {
        self.busy = false;
        self.finished = n;
        if !ok {
            self.failed = true;
        }
    }
    #[must_use]
    pub fn started(&self) -> u64 {
        self.started
    }
    #[must_use]
    pub fn finished(&self) -> u64 {
        self.finished
    }
    const fn word(&self) -> u64 {
        (self.started << 1) | self.busy as u64
    }
}
pub struct SharedBarrier {
    word: AtomicU64,
    finished: AtomicU64,
    failed: AtomicBool,
    first: Mutex<Option<Error>>,
    bell: Bell,
    barriers: Stat,
}
impl core::fmt::Debug for SharedBarrier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedBarrier")
            .field("barriers", &self.barriers)
            .field("waiters", if self.bell.scheduled() { &"parked" } else { &"blocked" })
            .finish_non_exhaustive()
    }
}
impl Default for SharedBarrier {
    fn default() -> Self {
        Self::new()
    }
}
struct Leading<'a> {
    barrier: &'a SharedBarrier,
    n: u64,
    done: bool,
}
impl Drop for Leading<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.barrier.complete(self.n, &Err(Error::Corrupt));
        }
    }
}
impl SharedBarrier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            first: Mutex::new(None),
            bell: Bell::new(),
            barriers: Stat::default(),
        }
    }
    pub fn park_on(&self, scheduler: Arc<dyn Scheduler>) -> bool {
        self.bell.park_on(scheduler)
    }
    fn snapshot(&self, join: bool) -> State {
        let word = if join {
            self.word.fetch_add(0, Ordering::AcqRel)
        } else {
            self.word.load(Ordering::Acquire)
        };
        let finished = self.finished.load(Ordering::Acquire);
        let failed = self.failed.load(Ordering::Acquire);
        State { started: word >> 1, finished, busy: word & 1 == 1, failed }
    }
    pub fn sync(&self, device: &dyn PageStore) -> Result<(), Error> {
        let need = self.snapshot(true).join();
        let mut armed: Option<Ticket> = None;
        let mut turns = 0u32;
        loop {
            let now = self.snapshot(false);
            match now.step(need) {
                Step::Released => {
                    self.hand_back(armed);
                    return Ok(());
                }
                Step::Refused => {
                    self.hand_back(armed);
                    return Err(self.cause());
                }
                Step::Lead(n) => {
                    let taken = self.word.compare_exchange(
                        now.word(),
                        now.begun(n).word(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    if taken.is_ok() {
                        self.hand_back(armed);
                        return self.lead(device, n);
                    }
                }
                Step::Wait => {
                    let watch = SPIN_TURNS;
                    if turns < watch {
                        turns += 1;
                        core::hint::spin_loop();
                        continue;
                    }
                    turns = 0;
                    self.park(now, &mut armed)?;
                }
            }
        }
    }
    fn park(&self, seen: State, armed: &mut Option<Ticket>) -> Result<(), Error> {
        self.bell.park(
            || {
                self.finished.load(Ordering::Acquire) != seen.finished
                    || self.word.load(Ordering::Acquire) != seen.word()
                    || self.failed.load(Ordering::Acquire)
            },
            armed,
        )
    }
    fn lead(&self, device: &dyn PageStore, n: u64) -> Result<(), Error> {
        let mut leading = Leading { barrier: self, n, done: false };
        let outcome = device.barrier();
        self.barriers.add(1);
        leading.done = true;
        self.complete(n, &outcome)?;
        outcome
    }
    fn complete(&self, n: u64, outcome: &Result<(), Error>) -> Result<(), Error> {
        if let Err(cause) = outcome {
            let mut first = self.first.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            first.get_or_insert_with(|| cause.clone());
        }
        let mut after =
            State { started: n, finished: n.saturating_sub(1), busy: true, failed: false };
        after.complete(n, outcome.is_ok());
        if after.failed {
            self.failed.store(true, Ordering::Release);
        }
        self.finished.store(after.finished, Ordering::Release);
        let _ = self.word.swap(after.word(), Ordering::AcqRel);
        self.bell.ring()
    }
    fn cause(&self) -> Error {
        self.first.lock().ok().and_then(|f| f.clone()).unwrap_or(Error::Corrupt)
    }
    fn hand_back(&self, armed: Option<Ticket>) {
        self.bell.hand_back(armed);
    }
    #[must_use]
    pub fn barriers(&self) -> u64 {
        self.barriers.get()
    }
    #[must_use]
    pub fn parks_and_wakes(&self) -> (u64, u64) {
        self.bell.parks_and_wakes()
    }
    #[must_use]
    pub fn state(&self) -> State {
        self.snapshot(false)
    }
}
