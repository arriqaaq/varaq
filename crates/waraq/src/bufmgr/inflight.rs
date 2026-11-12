use crate::PageId;
use crate::sync::{Condvar, Mutex};
use crate::task::{Scheduler, Ticket as Parked};
use std::sync::Arc;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Empty,
    Reading,
    Ready(u32),
    Failed(Gone),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    Failed,
    Unnamed,
    Full { capacity: usize },
    Livelocked { attempts: u32 },
}
#[derive(Debug, Clone, Copy)]
struct Entry {
    page: PageId,
    row: Row,
    holders: u32,
}
impl Entry {
    const EMPTY: Self = Self { page: PageId::CATALOG, row: Row::Empty, holders: 0 };
}
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a claim that is neither loaded nor waited on leaves a row nobody will retire"]
pub enum Claim {
    Load(Ticket),
    Wait(Ticket),
    TableFull,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Ticket {
    page: PageId,
}
#[derive(Debug)]
struct Table {
    rows: Box<[Entry]>,
    parked: Vec<(PageId, Parked)>,
    room_waiters: usize,
    sleepers: usize,
}
const ROOM: PageId = PageId::CATALOG;
#[repr(align(64))]
pub struct InflightTable {
    entries: Mutex<Table>,
    arrived: Condvar,
    scheduler: Option<Arc<dyn Scheduler>>,
}
#[allow(clippy::missing_fields_in_debug)]
impl core::fmt::Debug for InflightTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InflightTable")
            .field("entries", &self.entries)
            .field("waiters", if self.scheduler.is_some() { &"parked" } else { &"blocked" })
            .finish()
    }
}
impl InflightTable {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "an in-flight table with no capacity deduplicates nothing");
        let n = capacity.next_power_of_two();
        Self {
            entries: Mutex::new(Table {
                rows: vec![Entry::EMPTY; n].into_boxed_slice(),
                parked: Vec::new(),
                room_waiters: 0,
                sleepers: 0,
            }),
            arrived: Condvar::new(),
            scheduler: None,
        }
    }
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.rows.len())
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.rows.iter().filter(|x| x.row != Row::Empty).count())
    }
    #[must_use]
    pub fn holders(&self, page: PageId) -> u32 {
        let entries = self
            .entries
            .lock()
            .map_or(None, |e| Self::find(&e.rows, page).map(|slot| e.rows[slot].holders));
        entries.unwrap_or(0)
    }
    #[must_use]
    pub fn room_waiters(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.room_waiters)
    }
    pub fn wait_for_room(&self) -> bool {
        match self.scheduler.as_ref() {
            Some(scheduler) => self.wait_room_parked(scheduler.as_ref()),
            None => self.wait_room_blocking(),
        }
    }
    #[must_use]
    pub fn sleepers(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.sleepers)
    }
    fn full(rows: &[Entry]) -> bool {
        rows.iter().all(|e| e.row != Row::Empty)
    }
    fn wait_room_blocking(&self) -> bool {
        let Ok(mut entries) = self.entries.lock() else { return false };
        if !Self::full(&entries.rows) {
            return true;
        }
        entries.room_waiters += 1;
        entries.sleepers += 1;
        let Ok(mut entries) = self.arrived.wait(entries) else { return false };
        entries.sleepers -= 1;
        entries.room_waiters -= 1;
        true
    }
    fn wait_room_parked(&self, scheduler: &dyn Scheduler) -> bool {
        let parked = scheduler.arm();
        let Ok(mut entries) = self.entries.lock() else {
            Self::hand_back(scheduler, parked);
            return false;
        };
        if !Self::full(&entries.rows) {
            drop(entries);
            Self::hand_back(scheduler, parked);
            return true;
        }
        entries.parked.push((ROOM, parked));
        entries.room_waiters += 1;
        drop(entries);
        scheduler.wait(parked);
        let Ok(mut entries) = self.entries.lock() else { return false };
        entries.room_waiters -= 1;
        true
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn claim(&self, page: PageId) -> Claim {
        let Ok(mut entries) = self.entries.lock() else {
            return Claim::TableFull;
        };
        Self::register(&mut entries.rows, page)
    }
    fn register(rows: &mut [Entry], page: PageId) -> Claim {
        let mask = rows.len() - 1;
        let mut slot = Self::hash(page) & mask;
        for _ in 0..rows.len() {
            match rows[slot].row {
                Row::Empty => {
                    rows[slot] = Entry { page, row: Row::Reading, holders: 1 };
                    return Claim::Load(Ticket { page });
                }
                _ if rows[slot].page == page => {
                    rows[slot].holders += 1;
                    return Claim::Wait(Ticket { page });
                }
                _ => slot = (slot + 1) & mask,
            }
        }
        Claim::TableFull
    }
    fn find(entries: &[Entry], page: PageId) -> Option<usize> {
        let mask = entries.len() - 1;
        let mut slot = Self::hash(page) & mask;
        for _ in 0..entries.len() {
            match entries[slot].row {
                Row::Empty => return None,
                _ if entries[slot].page == page => return Some(slot),
                _ => slot = (slot + 1) & mask,
            }
        }
        None
    }
    pub fn publish(&self, ticket: &Ticket, frame: u32) {
        let mut woken = Vec::new();
        let mut asleep = true;
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(slot) = Self::find(&entries.rows, ticket.page) {
                entries.rows[slot].row = Row::Ready(frame);
            }
            woken = Self::take_parked(&mut entries, ticket.page);
            asleep = entries.sleepers > 0;
        }
        self.notify_if(asleep);
        self.wake_parked(woken);
    }
    fn notify_if(&self, asleep: bool) {
        if asleep {
            self.arrived.notify_all();
        }
    }
    pub fn abandon(&self, ticket: &Ticket, gone: Gone) {
        let mut woken = Vec::new();
        let mut asleep = true;
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(slot) = Self::find(&entries.rows, ticket.page) {
                entries.rows[slot].row = Row::Failed(gone);
            }
            woken = Self::take_parked(&mut entries, ticket.page);
            asleep = entries.sleepers > 0;
        }
        self.notify_if(asleep);
        self.wake_parked(woken);
    }
    #[allow(clippy::needless_pass_by_value)]
    pub fn wait(&self, ticket: Ticket) -> Result<u32, Gone> {
        match self.scheduler.as_ref() {
            Some(scheduler) => self.wait_parked(ticket, scheduler.as_ref()),
            None => self.wait_blocking(&ticket),
        }
    }
    #[allow(clippy::needless_pass_by_value)]
    fn wait_parked(&self, ticket: Ticket, scheduler: &dyn Scheduler) -> Result<u32, Gone> {
        loop {
            let parked = scheduler.arm();
            let Ok(mut entries) = self.entries.lock() else {
                Self::hand_back(scheduler, parked);
                return Err(Gone::Failed);
            };
            let Some(slot) = Self::find(&entries.rows, ticket.page) else {
                debug_assert!(false, "a row was retired while a holder was registered on it");
                drop(entries);
                Self::hand_back(scheduler, parked);
                return Err(Gone::Failed);
            };
            match entries.rows[slot].row {
                Row::Ready(frame) => {
                    Self::release(&mut entries.rows, &ticket);
                    drop(entries);
                    Self::hand_back(scheduler, parked);
                    return Ok(frame);
                }
                Row::Failed(gone) => {
                    Self::release(&mut entries.rows, &ticket);
                    drop(entries);
                    Self::hand_back(scheduler, parked);
                    return Err(gone);
                }
                _ => {
                    entries.parked.push((ticket.page, parked));
                    drop(entries);
                    scheduler.wait(parked);
                }
            }
        }
    }
    fn hand_back(scheduler: &dyn Scheduler, parked: Parked) {
        scheduler.wake(parked);
        scheduler.wait(parked);
    }
    fn take_parked(entries: &mut Table, page: PageId) -> Vec<Parked> {
        let mut woken = Vec::new();
        entries.parked.retain(|(p, t)| {
            if *p == page {
                woken.push(*t);
                false
            } else {
                true
            }
        });
        woken
    }
    fn wake_parked(&self, woken: Vec<Parked>) {
        if let Some(scheduler) = self.scheduler.as_ref() {
            for ticket in woken {
                scheduler.wake(ticket);
            }
        }
    }
    fn wait_blocking(&self, ticket: &Ticket) -> Result<u32, Gone> {
        let Ok(mut entries) = self.entries.lock() else {
            return Err(Gone::Failed);
        };
        loop {
            let Some(slot) = Self::find(&entries.rows, ticket.page) else {
                debug_assert!(false, "a row was retired while a holder was registered on it");
                return Err(Gone::Failed);
            };
            match entries.rows[slot].row {
                Row::Ready(frame) => {
                    Self::release(&mut entries.rows, ticket);
                    return Ok(frame);
                }
                Row::Failed(gone) => {
                    Self::release(&mut entries.rows, ticket);
                    return Err(gone);
                }
                _ => {
                    entries.sleepers += 1;
                    let Ok(mut next) = self.arrived.wait(entries) else {
                        return Err(Gone::Failed);
                    };
                    next.sleepers -= 1;
                    entries = next;
                }
            }
        }
    }
    #[allow(clippy::needless_pass_by_value)]
    pub fn leave(&self, ticket: Ticket) {
        let mut woken = Vec::new();
        let mut asleep = true;
        if let Ok(mut entries) = self.entries.lock() {
            Self::release(&mut entries.rows, &ticket);
            woken = Self::take_parked(&mut entries, ticket.page);
            woken.extend(Self::take_parked(&mut entries, ROOM));
            asleep = entries.sleepers > 0;
        }
        self.notify_if(asleep);
        self.wake_parked(woken);
    }
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Arc<dyn Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }
    fn release(entries: &mut [Entry], ticket: &Ticket) {
        let Some(slot) = Self::find(entries, ticket.page) else { return };
        entries[slot].holders = entries[slot].holders.saturating_sub(1);
        if entries[slot].holders == 0 {
            Self::retire(entries, slot);
        }
    }
    fn retire(entries: &mut [Entry], slot: usize) {
        let mask = entries.len() - 1;
        entries[slot] = Entry::EMPTY;
        let mut hole = slot;
        let mut probe = (hole + 1) & mask;
        while entries[probe].row != Row::Empty {
            let ideal = Self::hash(entries[probe].page) & mask;
            let from_ideal = probe.wrapping_sub(ideal) & mask;
            let hole_from_ideal = hole.wrapping_sub(ideal) & mask;
            if hole_from_ideal <= from_ideal {
                entries[hole] = entries[probe];
                entries[probe] = Entry::EMPTY;
                hole = probe;
            }
            probe = (probe + 1) & mask;
        }
    }
    fn hash(page: PageId) -> usize {
        const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
        (page.get().wrapping_mul(GOLDEN) >> 32) as usize
    }
}
