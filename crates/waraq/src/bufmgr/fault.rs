use crate::bufmgr::inflight::{Claim, Gone};
use crate::bufmgr::pool::Pool;
use crate::node::Node;
use crate::store::PageStore;
use crate::{Error, PAGE_SIZE, PageId};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    ReadFailed,
    NoFrame,
    Unnamed,
}
pub type Name<'a> = &'a dyn Fn(PageId, u32, Route) -> Result<u32, Error>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Held,
    Parent { frame: u32, version: u64 },
    Root { version: u64 },
}
pub type Named<'a> = &'a dyn Fn(PageId) -> Option<u32>;
pub fn fault(pool: &Pool, store: &dyn PageStore, page: PageId) -> Result<u32, Error> {
    fault_named(pool, store, page, Route::Held, &|_| None, &|_, frame, _| Ok(frame))
}
pub fn fault_named(
    pool: &Pool,
    store: &dyn PageStore,
    page: PageId,
    route: Route,
    named: Named<'_>,
    name: Name<'_>,
) -> Result<u32, Error> {
    for _ in 0..crate::tree::RETRY_BOUND {
        match pool.claim_read(page) {
            Claim::Load(ticket) => {
                if let Some(frame) = named(page) {
                    let table = pool.reads(page);
                    table.publish(&ticket, frame);
                    table.leave(ticket);
                    return Ok(frame);
                }
                let outcome = load(pool, store, page).and_then(|frame| {
                    name(page, frame, route).inspect_err(|_| unnamed_back(pool, page, frame))
                });
                let table = pool.reads(page);
                match &outcome {
                    Ok(frame) => table.publish(&ticket, *frame),
                    Err(e) => table.abandon(&ticket, gone(e)),
                }
                table.leave(ticket);
                return outcome;
            }
            Claim::Wait(ticket) => {
                return pool.reads(page).wait(ticket).map_err(|g| waited(page, g));
            }
            Claim::TableFull => {
                if !pool.reads(page).wait_for_room() {
                    return Err(Error::Poisoned);
                }
            }
        }
    }
    Err(Error::Livelocked { attempts: crate::tree::RETRY_BOUND })
}
pub fn fault_span(
    pool: &Pool,
    store: &dyn PageStore,
    first: PageId,
    count: usize,
    route: Route,
    named: Named<'_>,
    name: Name<'_>,
) -> Result<usize, Error> {
    let mut claimed: Vec<(PageId, usize, crate::bufmgr::inflight::Ticket)> = Vec::new();
    for i in 0..count {
        let Ok(page) = PageId::new(first.get().saturating_add(i as u64)) else { break };
        match pool.claim_read(page) {
            Claim::Load(ticket) => {
                if let Some(frame) = named(page) {
                    let table = pool.reads(page);
                    table.publish(&ticket, frame);
                    table.leave(ticket);
                } else {
                    claimed.push((page, i, ticket));
                }
            }
            Claim::Wait(ticket) => pool.reads(page).leave(ticket),
            Claim::TableFull => {}
        }
    }
    let (Some(&(_, lo, _)), Some(&(_, last, _))) = (claimed.first(), claimed.last()) else {
        return Ok(0);
    };
    thread_local! {
        static SPAN : core::cell::Cell < Vec < u8 >> = const {
        core::cell::Cell::new(Vec::new()) };
    }
    let mut bytes = SPAN.with(core::cell::Cell::take);
    let len = (last + 1 - lo) * PAGE_SIZE;
    if bytes.len() < len {
        bytes.resize(len, 0);
    }
    let read = PageId::new(first.get().saturating_add(lo as u64))
        .and_then(|from| store.read_run(from, &mut bytes[..len]));
    let mut loaded = 0;
    let mut first_refusal = None;
    for (page, i, ticket) in claimed {
        let at = (i - lo) * PAGE_SIZE;
        let own = match &read {
            Ok(()) => Ok(()),
            Err(_) => store.read(page, &mut bytes[at..at + PAGE_SIZE]),
        };
        if let Err(e) = &own {
            first_refusal.get_or_insert_with(|| e.clone());
        }
        let outcome = own.and_then(|()| {
            let page_bytes: &[u8; PAGE_SIZE] =
                bytes[at..at + PAGE_SIZE].try_into().map_err(|_| Error::Corrupt)?;
            let index = pool.acquire(page).ok_or(Error::Full { capacity: pool.len() })?;
            install(pool, index, page, page_bytes).and_then(|frame| {
                name(page, frame, route).inspect_err(|_| unnamed_back(pool, page, frame))
            })
        });
        let table = pool.reads(page);
        match &outcome {
            Ok(frame) => {
                table.publish(&ticket, *frame);
                loaded += 1;
            }
            Err(e) => table.abandon(&ticket, gone(e)),
        }
        table.leave(ticket);
    }
    SPAN.with(|cell| cell.set(bytes));
    first_refusal.map_or(Ok(loaded), Err)
}
fn load(pool: &Pool, store: &dyn PageStore, page: PageId) -> Result<u32, Error> {
    let Some(index) = pool.acquire(page) else {
        return Err(Error::Full { capacity: pool.len() });
    };
    let mut bytes = [0_u8; PAGE_SIZE];
    if let Err(e) = store.read(page, &mut bytes) {
        release_frame(pool, index);
        return Err(e);
    }
    install(pool, index, page, &bytes)
}
fn install(pool: &Pool, index: u32, page: PageId, bytes: &[u8; PAGE_SIZE]) -> Result<u32, Error> {
    let node = Node::from_page_bytes(bytes);
    if !node.is_well_formed() {
        release_frame(pool, index);
        return Err(Error::PageCorrupt { pid: page, stored: 0, computed: 0 });
    }
    let Some(slot) = pool.slot(index) else {
        release_frame(pool, index);
        return Err(Error::Corrupt);
    };
    let disk_gsn = node.gsn();
    let Some(mut guard) = latch_claimed(&slot.page) else {
        release_frame(pool, index);
        return Err(Error::Livelocked { attempts: crate::tree::RETRY_BOUND });
    };
    guard.set(node);
    drop(guard);
    if slot.frame.arrive(disk_gsn).is_err() {
        release_frame(pool, index);
        return Err(Error::Corrupt);
    }
    Ok(index)
}
pub struct WorkerChannel {
    channel: crate::sync::Mutex<Box<dyn crate::store::channel::IoChannel>>,
}
impl WorkerChannel {
    #[must_use]
    pub fn new(channel: Box<dyn crate::store::channel::IoChannel>) -> Self {
        let this = Self { channel: crate::sync::Mutex::new(channel) };
        drop(this.channel.lock());
        this
    }
}
impl std::fmt::Debug for WorkerChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerChannel").finish_non_exhaustive()
    }
}
#[derive(Debug)]
struct Loading {
    page: PageId,
    route: Route,
    frame: u32,
    ticket: crate::bufmgr::inflight::Ticket,
}
pub fn fault_via(
    pool: &Pool,
    channel: &WorkerChannel,
    page: PageId,
    route: Route,
    named: Named<'_>,
    name: Name<'_>,
) -> Result<u32, Error> {
    for _ in 0..crate::tree::RETRY_BOUND {
        match pool.claim_read(page) {
            Claim::Load(ticket) => {
                if let Some(frame) = named(page) {
                    let table = pool.reads(page);
                    table.publish(&ticket, frame);
                    table.leave(ticket);
                    return Ok(frame);
                }
                let Ok(held) = channel.channel.lock() else {
                    abandon(pool, page, ticket, &Error::Poisoned);
                    return Err(Error::Poisoned);
                };
                return submit_and_drive(pool, &**held, (page, route), ticket, name);
            }
            Claim::Wait(ticket) => {
                return pool.reads(page).wait(ticket).map_err(|g| waited(page, g));
            }
            Claim::TableFull => {
                if !pool.reads(page).wait_for_room() {
                    return Err(Error::Poisoned);
                }
            }
        }
    }
    Err(Error::Livelocked { attempts: crate::tree::RETRY_BOUND })
}
fn submit_and_drive(
    pool: &Pool,
    channel: &dyn crate::store::channel::IoChannel,
    (page, route): (PageId, Route),
    ticket: crate::bufmgr::inflight::Ticket,
    name: Name<'_>,
) -> Result<u32, Error> {
    let Some(frame) = pool.acquire(page) else {
        let full = Error::Full { capacity: pool.len() };
        abandon(pool, page, ticket, &full);
        return Err(full);
    };
    let mine = Loading { page, route, frame, ticket };
    let buf = vec![0_u8; PAGE_SIZE].into_boxed_slice();
    let id = match channel.push_read(page, buf) {
        Ok(id) => id,
        Err(e) => return Err(give_up(pool, mine, e)),
    };
    if let Err(e) = channel.submit() {
        return Err(give_up(pool, mine, e));
    }
    loop {
        let done = match channel.poll(1) {
            Ok(done) => done,
            Err(e) => return Err(give_up(pool, mine, e)),
        };
        if done.is_empty() {
            return Err(give_up(pool, mine, Error::FaultFailed { page, why: Why::ReadFailed }));
        }
        if let Some(completion) = done.into_iter().find(|c| c.id == id) {
            return finish(pool, mine, completion, name);
        }
    }
}
fn give_up(pool: &Pool, mine: Loading, e: Error) -> Error {
    release_frame(pool, mine.frame);
    abandon(pool, mine.page, mine.ticket, &e);
    e
}
fn finish(
    pool: &Pool,
    mine: Loading,
    completion: crate::store::channel::Completion,
    name: Name<'_>,
) -> Result<u32, Error> {
    let Loading { page, route, frame, ticket } = mine;
    let mut arrived = false;
    let outcome: Result<u32, Error> = (|| {
        completion.outcome?;
        let image: &[u8; PAGE_SIZE] = completion
            .buf
            .as_ref()
            .try_into()
            .map_err(|_| Error::PageCorrupt { pid: page, stored: 0, computed: 0 })?;
        crate::page::checksum::verify(image, page)?;
        let node = Node::from_page_bytes(image);
        if !node.is_well_formed() {
            return Err(Error::PageCorrupt { pid: page, stored: 0, computed: 0 });
        }
        let slot = pool.slot(frame).ok_or(Error::Corrupt)?;
        let disk_gsn = node.gsn();
        latch_claimed(&slot.page)
            .ok_or(Error::Livelocked { attempts: crate::tree::RETRY_BOUND })?
            .set(node);
        slot.frame.arrive(disk_gsn).map_err(|_| Error::Corrupt)?;
        arrived = true;
        name(page, frame, route)
    })();
    let table = pool.reads(page);
    match &outcome {
        Ok(f) => table.publish(&ticket, *f),
        Err(e) => {
            if arrived {
                unnamed_back(pool, page, frame);
            } else {
                release_frame(pool, frame);
            }
            table.abandon(&ticket, gone(e));
        }
    }
    table.leave(ticket);
    outcome
}
pub(crate) fn latch_claimed(
    latch: &crate::bufmgr::PageLatch,
) -> Option<crate::bufmgr::pool::PageGuard<'_>> {
    for _ in 0..crate::tree::RETRY_BOUND {
        if let Ok(guard) = latch.claim() {
            return Some(guard);
        }
    }
    None
}
pub(crate) fn release_frame(pool: &Pool, index: u32) {
    if pool.slot(index).is_some_and(|s| s.frame.abandon_load().is_ok()) {
        let _ = pool.release(index);
    }
}
pub(crate) fn unnamed_back(pool: &Pool, page: PageId, index: u32) {
    let Some(slot) = pool.slot(index) else { return };
    for _ in 0..crate::tree::RETRY_BOUND {
        if let Ok(guard) = slot.page.claim() {
            if slot.frame.page_id() == Some(page) && slot.frame.reclaim().is_ok() {
                drop(guard);
                let _ = pool.release(index);
            }
            return;
        }
        if slot.frame.page_id() != Some(page) || !slot.frame.state().is_readable() {
            return;
        }
        crate::latch::spin();
    }
}
fn abandon(pool: &Pool, page: PageId, ticket: crate::bufmgr::inflight::Ticket, why: &Error) {
    let table = pool.reads(page);
    table.abandon(&ticket, gone(why));
    table.leave(ticket);
}
fn gone(e: &Error) -> Gone {
    match *e {
        Error::FaultFailed { why: Why::Unnamed, .. } => Gone::Unnamed,
        Error::Full { capacity } => Gone::Full { capacity },
        Error::Livelocked { attempts } => Gone::Livelocked { attempts },
        _ => Gone::Failed,
    }
}
fn waited(page: PageId, gone: Gone) -> Error {
    match gone {
        Gone::Failed => Error::FaultFailed { page, why: Why::ReadFailed },
        Gone::Unnamed => Error::FaultFailed { page, why: Why::Unnamed },
        Gone::Full { capacity } => Error::Full { capacity },
        Gone::Livelocked { attempts } => Error::Livelocked { attempts },
    }
}
