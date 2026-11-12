use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use varaq::{Scheduler, Ticket};
const SLOTS: usize = 64;
const SLOT_BITS: u32 = 16;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
const GENERATION_MASK: u64 = u64::MAX >> SLOT_BITS;
#[derive(Debug)]
struct SlotState {
    generation: u64,
    woken: bool,
    held: bool,
}
#[derive(Debug)]
struct Slot {
    state: Mutex<SlotState>,
    bell: Condvar,
}
impl Slot {
    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
#[derive(Debug)]
pub struct ThreadScheduler {
    slots: Vec<Slot>,
    free: Mutex<Vec<usize>>,
    vacancy: Condvar,
    next: AtomicU64,
}
impl Default for ThreadScheduler {
    fn default() -> Self {
        Self::new()
    }
}
impl ThreadScheduler {
    #[must_use]
    pub fn new() -> Self {
        const { assert!(SLOTS <= (SLOT_MASK as usize) + 1, "a slot index must fit in SLOT_BITS") }
        Self {
            slots: (0..SLOTS)
                .map(|_| Slot {
                    state: Mutex::new(SlotState { generation: 0, woken: false, held: false }),
                    bell: Condvar::new(),
                })
                .collect(),
            free: Mutex::new((0..SLOTS).collect()),
            vacancy: Condvar::new(),
            next: AtomicU64::new(1),
        }
    }
    fn index_of(ticket: Ticket) -> usize {
        (ticket.0 & SLOT_MASK) as usize
    }
    fn generation_of(ticket: Ticket) -> u64 {
        ticket.0 >> SLOT_BITS
    }
}
impl Scheduler for ThreadScheduler {
    fn arm(&self) -> Ticket {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        let index = loop {
            if let Some(index) = free.pop() {
                break index;
            }
            free = self.vacancy.wait(free).unwrap_or_else(PoisonError::into_inner);
        };
        drop(free);
        let generation = self.next.fetch_add(1, Ordering::Relaxed) & GENERATION_MASK;
        let slot = &self.slots[index];
        let mut state = slot.state();
        state.generation = generation;
        state.woken = false;
        state.held = true;
        drop(state);
        Ticket((generation << SLOT_BITS) | (index as u64))
    }
    fn wait(&self, ticket: Ticket) {
        let index = Self::index_of(ticket);
        let slot = &self.slots[index];
        let mut state = slot.state();
        while !state.woken {
            state = slot.bell.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        state.held = false;
        drop(state);
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        free.push(index);
        drop(free);
        self.vacancy.notify_one();
    }
    fn wake(&self, ticket: Ticket) {
        let slot = &self.slots[Self::index_of(ticket)];
        let mut state = slot.state();
        if state.generation != Self::generation_of(ticket) {
            return;
        }
        state.woken = true;
        drop(state);
        slot.bell.notify_all();
    }
    fn yield_now(&self) {
        std::thread::yield_now();
    }
}
