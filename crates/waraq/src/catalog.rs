use crate::Error;
use crate::store::PageStore;
use crate::{PageId, page::checksum};
pub const FIRST_SLOT: u64 = 1;
pub const SECOND_SLOT: u64 = 2;
pub const HEAD_BYTES: usize = offsets::WORKERS_AT;
pub const MAX_WORKERS: usize = (crate::PAGE_SIZE - HEAD_BYTES) / PER_WORKER_BYTES;
const PER_WORKER_BYTES: usize = 8 + 8 + 8;
const _: () = assert!(HEAD_BYTES + (MAX_WORKERS + 1) * PER_WORKER_BYTES > crate::PAGE_SIZE);
pub mod offsets {
    pub const CHECKSUM: usize = 0;
    pub const CATALOG_IDENTITY: usize = 4;
    pub const PAGE_IDENTITY: usize = 8;
    pub const VALUE_IDENTITY: usize = 12;
    pub const LOG_IDENTITY: usize = 16;
    pub const WORKERS: usize = 20;
    pub const PADDING: usize = 24;
    pub const SEQUENCE: usize = 28;
    pub const RING_BASE: usize = 36;
    pub const RING_CAPACITY: usize = 44;
    pub const ROOT: usize = 52;
    pub const HIGHEST_PAGE: usize = 60;
    pub const TXN_CLOCK: usize = 68;
    pub const GSN_CLOCK: usize = 76;
    pub const DATA_PAGES: usize = 84;
    pub const FREE_PAGES: usize = 92;
    pub const STAMP_CEILING: usize = 100;
    pub const BLOB_SERIAL: usize = 108;
    pub const BLOB_FREE_PAGES: usize = 116;
    pub const WORKERS_AT: usize = 124;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Worker {
    pub begin: u64,
    pub flushed: u64,
    pub committed: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    pub sequence: u64,
    pub ring_base: u64,
    pub ring_capacity: u64,
    pub root: u64,
    pub highest_page: u64,
    pub txn_clock: u64,
    pub gsn_clock: u64,
    pub data_pages: u64,
    pub free_pages: u64,
    pub stamp_ceiling: u64,
    pub blob_serial: u64,
    pub blob_free_pages: u64,
    pub workers: Vec<Worker>,
}
pub const IDENTITY: u32 = 0xC0DE_0005;
impl Catalog {
    #[must_use]
    pub fn len(&self) -> usize {
        HEAD_BYTES + self.workers.len() * PER_WORKER_BYTES
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }
    pub fn write(&self, slot: PageId, into: &mut [u8]) -> Result<(), Error> {
        if self.workers.is_empty() || self.workers.len() > MAX_WORKERS {
            return Err(Error::Corrupt);
        }
        if into.len() != crate::PAGE_SIZE || self.len() > crate::PAGE_SIZE {
            return Err(Error::Corrupt);
        }
        into.fill(0);
        let put32 = |into: &mut [u8], at: usize, v: u32| {
            into[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        let put64 = |into: &mut [u8], at: usize, v: u64| {
            into[at..at + 8].copy_from_slice(&v.to_le_bytes());
        };
        put32(into, offsets::CATALOG_IDENTITY, IDENTITY);
        put32(into, offsets::PAGE_IDENTITY, crate::format::identity());
        put32(into, offsets::VALUE_IDENTITY, crate::format::value::identity());
        put32(into, offsets::LOG_IDENTITY, crate::format::log::identity());
        put32(
            into,
            offsets::WORKERS,
            u32::try_from(self.workers.len()).map_err(|_| Error::Corrupt)?,
        );
        put64(into, offsets::SEQUENCE, self.sequence);
        put64(into, offsets::RING_BASE, self.ring_base);
        put64(into, offsets::RING_CAPACITY, self.ring_capacity);
        put64(into, offsets::ROOT, self.root);
        put64(into, offsets::HIGHEST_PAGE, self.highest_page);
        put64(into, offsets::TXN_CLOCK, self.txn_clock);
        put64(into, offsets::GSN_CLOCK, self.gsn_clock);
        put64(into, offsets::DATA_PAGES, self.data_pages);
        put64(into, offsets::FREE_PAGES, self.free_pages);
        put64(into, offsets::STAMP_CEILING, self.stamp_ceiling);
        put64(into, offsets::BLOB_SERIAL, self.blob_serial);
        put64(into, offsets::BLOB_FREE_PAGES, self.blob_free_pages);
        for (i, w) in self.workers.iter().enumerate() {
            let at = offsets::WORKERS_AT + i * PER_WORKER_BYTES;
            put64(into, at, w.begin);
            put64(into, at + 8, w.flushed);
            put64(into, at + 16, w.committed);
        }
        let crc = slot_checksum(into, slot);
        put32(into, offsets::CHECKSUM, crc);
        Ok(())
    }
    pub fn read(slot: PageId, bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < crate::PAGE_SIZE {
            return Err(Error::Corrupt);
        }
        let page = &bytes[..crate::PAGE_SIZE];
        let get32 = |at: usize| -> u32 {
            u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
        };
        let get64 = |at: usize| -> u64 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&page[at..at + 8]);
            u64::from_le_bytes(b)
        };
        if get32(offsets::CHECKSUM) != slot_checksum(page, slot) {
            return Err(Error::Corrupt);
        }
        if get32(offsets::CATALOG_IDENTITY) != IDENTITY
            || get32(offsets::PAGE_IDENTITY) != crate::format::identity()
            || get32(offsets::VALUE_IDENTITY) != crate::format::value::identity()
            || get32(offsets::LOG_IDENTITY) != crate::format::log::identity()
        {
            return Err(Error::Corrupt);
        }
        if page[offsets::PADDING..offsets::PADDING + 4] != [0, 0, 0, 0] {
            return Err(Error::Corrupt);
        }
        let workers = get32(offsets::WORKERS) as usize;
        if workers == 0 || workers > MAX_WORKERS {
            return Err(Error::Corrupt);
        }
        if HEAD_BYTES + workers * PER_WORKER_BYTES > crate::PAGE_SIZE {
            return Err(Error::Corrupt);
        }
        let mut out = Self {
            sequence: get64(offsets::SEQUENCE),
            ring_base: get64(offsets::RING_BASE),
            ring_capacity: get64(offsets::RING_CAPACITY),
            root: get64(offsets::ROOT),
            highest_page: get64(offsets::HIGHEST_PAGE),
            txn_clock: get64(offsets::TXN_CLOCK),
            gsn_clock: get64(offsets::GSN_CLOCK),
            data_pages: get64(offsets::DATA_PAGES),
            free_pages: get64(offsets::FREE_PAGES),
            stamp_ceiling: get64(offsets::STAMP_CEILING),
            blob_serial: get64(offsets::BLOB_SERIAL),
            blob_free_pages: get64(offsets::BLOB_FREE_PAGES),
            workers: Vec::with_capacity(workers),
        };
        for i in 0..workers {
            let at = offsets::WORKERS_AT + i * PER_WORKER_BYTES;
            out.workers.push(Worker {
                begin: get64(at),
                flushed: get64(at + 8),
                committed: get64(at + 16),
            });
        }
        Ok(out)
    }
}
fn slot_checksum(page: &[u8], slot: PageId) -> u32 {
    mixed_with_slot(checksum::crc32c(&page[offsets::CATALOG_IDENTITY..crate::PAGE_SIZE]), slot)
}
fn mixed_with_slot(body: u32, slot: PageId) -> u32 {
    let mut mixed = [0u8; 12];
    mixed[..4].copy_from_slice(&body.to_le_bytes());
    mixed[4..].copy_from_slice(&slot.get().to_le_bytes());
    checksum::crc32c(&mixed)
}
#[must_use]
pub const fn slot_for(sequence: u64) -> u64 {
    if sequence.is_multiple_of(2) { FIRST_SLOT } else { SECOND_SLOT }
}
pub fn prepare(device: &dyn PageStore) -> Result<(), Error> {
    if !device.is_empty() {
        return Err(Error::Corrupt);
    }
    for want in FIRST_SLOT..=SECOND_SLOT {
        if device.allocate()?.get() != want {
            return Err(Error::Corrupt);
        }
    }
    Ok(())
}
pub fn is_blank(device: &dyn PageStore) -> Result<bool, Error> {
    if (device.len() as u64) < SECOND_SLOT {
        return Err(Error::Corrupt);
    }
    let mut page = vec![0u8; crate::PAGE_SIZE];
    for at in [FIRST_SLOT, SECOND_SLOT] {
        device.read_raw(PageId::new(at)?, &mut page)?;
        if page.iter().any(|&b| b != 0) {
            return Ok(false);
        }
    }
    Ok(true)
}
pub fn load(device: &dyn PageStore) -> Result<Catalog, Error> {
    let mut best: Option<Catalog> = None;
    let mut page = vec![0u8; crate::PAGE_SIZE];
    for at in [FIRST_SLOT, SECOND_SLOT] {
        let Ok(slot) = PageId::new(at) else { continue };
        if device.read_raw(slot, &mut page).is_err() {
            continue;
        }
        let Ok(found) = Catalog::read(slot, &page) else { continue };
        if best.as_ref().is_none_or(|b| found.sequence > b.sequence) {
            best = Some(found);
        }
    }
    best.ok_or(Error::Corrupt)
}
const FREE_PER_PAGE: usize = (crate::PAGE_SIZE - 16) / 16;
fn free_page(sequence: u64, i: u64) -> Result<PageId, Error> {
    let raw = i
        .checked_mul(2)
        .and_then(|at| at.checked_add(3 + (slot_for(sequence) - FIRST_SLOT)))
        .ok_or(Error::Corrupt)?;
    PageId::new(raw)
}
pub fn store_free(
    device: &dyn PageStore,
    sequence: u64,
    free: &[(u64, u64)],
) -> Result<u64, Error> {
    let mut page = vec![0u8; crate::PAGE_SIZE];
    let mut written = 0;
    for chunk in free.chunks(FREE_PER_PAGE) {
        let id = free_page(sequence, written)?;
        while (device.len() as u64) < id.get() {
            if device.allocate().is_err() {
                return Ok(written);
            }
        }
        page.fill(0);
        page[4..8].copy_from_slice(
            &u32::try_from(chunk.len()).map_err(|_| Error::Corrupt)?.to_le_bytes(),
        );
        page[8..16].copy_from_slice(&sequence.to_le_bytes());
        for (n, &(page_id, gsn)) in chunk.iter().enumerate() {
            let at = 16 + n * 16;
            page[at..at + 8].copy_from_slice(&page_id.to_le_bytes());
            page[at + 8..at + 16].copy_from_slice(&gsn.to_le_bytes());
        }
        checksum::stamp(&mut page, id);
        device.write(id, &page)?;
        written += 1;
    }
    Ok(written)
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Free {
    pub ids: Vec<(u64, u64)>,
    pub runs: Vec<(u64, u64, u64)>,
}
const RUNS_PER_PAGE: usize = (crate::PAGE_SIZE - 16) / 24;
pub fn store_runs(
    device: &dyn PageStore,
    sequence: u64,
    from: u64,
    runs: &[(u64, u64, u64)],
) -> Result<u64, Error> {
    let mut page = vec![0u8; crate::PAGE_SIZE];
    let mut written = 0;
    for chunk in runs.chunks(RUNS_PER_PAGE) {
        let id = free_page(sequence, from + written)?;
        while (device.len() as u64) < id.get() {
            if device.allocate().is_err() {
                return Ok(written);
            }
        }
        page.fill(0);
        page[4..8].copy_from_slice(
            &u32::try_from(chunk.len()).map_err(|_| Error::Corrupt)?.to_le_bytes(),
        );
        page[8..16].copy_from_slice(&sequence.to_le_bytes());
        for (n, &(first, pages, stamp)) in chunk.iter().enumerate() {
            let at = 16 + n * 24;
            page[at..at + 8].copy_from_slice(&first.to_le_bytes());
            page[at + 8..at + 16].copy_from_slice(&pages.to_le_bytes());
            page[at + 16..at + 24].copy_from_slice(&stamp.to_le_bytes());
        }
        checksum::stamp(&mut page, id);
        device.write(id, &page)?;
        written += 1;
    }
    Ok(written)
}
#[must_use]
pub fn load_runs(device: &dyn PageStore, catalog: &Catalog) -> Vec<(u64, u64, u64)> {
    let mut page = vec![0u8; crate::PAGE_SIZE];
    let mut out = Vec::new();
    for i in catalog.free_pages..catalog.free_pages.saturating_add(catalog.blob_free_pages) {
        let Ok(id) = free_page(catalog.sequence, i) else { break };
        if id.get() > device.len() as u64 {
            break;
        }
        if device.read(id, &mut page).is_err() {
            continue;
        }
        let count = u32::from_le_bytes([page[4], page[5], page[6], page[7]]) as usize;
        let mut seq = [0u8; 8];
        seq.copy_from_slice(&page[8..16]);
        if u64::from_le_bytes(seq) != catalog.sequence || count > RUNS_PER_PAGE {
            continue;
        }
        let word = |at: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&page[at..at + 8]);
            u64::from_le_bytes(w)
        };
        for n in 0..count {
            let at = 16 + n * 24;
            out.push((word(at), word(at + 8), word(at + 16)));
        }
    }
    out
}
#[must_use]
pub fn load_free(device: &dyn PageStore, catalog: &Catalog) -> Vec<(u64, u64)> {
    let mut page = vec![0u8; crate::PAGE_SIZE];
    let mut out = Vec::new();
    for i in 0..catalog.free_pages {
        let Ok(id) = free_page(catalog.sequence, i) else { break };
        if id.get() > device.len() as u64 {
            break;
        }
        if device.read(id, &mut page).is_err() {
            continue;
        }
        let count = u32::from_le_bytes([page[4], page[5], page[6], page[7]]) as usize;
        let mut seq = [0u8; 8];
        seq.copy_from_slice(&page[8..16]);
        if u64::from_le_bytes(seq) != catalog.sequence || count > FREE_PER_PAGE {
            continue;
        }
        for n in 0..count {
            let at = 16 + n * 16;
            let mut a = [0u8; 8];
            let mut b = [0u8; 8];
            a.copy_from_slice(&page[at..at + 8]);
            b.copy_from_slice(&page[at + 8..at + 16]);
            out.push((u64::from_le_bytes(a), u64::from_le_bytes(b)));
        }
    }
    out
}
pub fn store(device: &dyn PageStore, catalog: &Catalog) -> Result<(), Error> {
    let slot = PageId::new(slot_for(catalog.sequence))?;
    let mut page = vec![0u8; crate::PAGE_SIZE];
    catalog.write(slot, &mut page)?;
    device.write(slot, &page)?;
    device.barrier()
}
pub const STAMP_CEILING_STEP: u64 = 1 << 32;
pub struct Keeper {
    device: Box<dyn PageStore>,
    writing: crate::sync::Mutex<()>,
    ceiling: core::sync::atomic::AtomicU64,
}
impl core::fmt::Debug for Keeper {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keeper").field("ceiling", &self.ceiling()).finish_non_exhaustive()
    }
}
impl Keeper {
    #[must_use]
    pub fn new(device: Box<dyn PageStore>, ceiling: u64) -> Self {
        Self {
            device,
            writing: crate::sync::Mutex::new(()),
            ceiling: core::sync::atomic::AtomicU64::new(ceiling),
        }
    }
    #[must_use]
    pub fn device(&self) -> &dyn PageStore {
        self.device.as_ref()
    }
    #[must_use]
    pub fn ceiling(&self) -> u64 {
        self.ceiling.load(core::sync::atomic::Ordering::Acquire)
    }
    pub fn write(
        &self,
        make: impl FnOnce(Option<&Catalog>) -> Result<(Catalog, Free), Error>,
    ) -> Result<(), Error> {
        let _writing = self.writing.lock().map_err(|_| Error::Corrupt)?;
        let previous = load(self.device()).ok();
        let (recorded, free) = make(previous.as_ref())?;
        self.put(previous.as_ref(), recorded, &free)
    }
    pub fn raise(&self, stamp: u64) -> Result<(), Error> {
        if stamp < self.ceiling() {
            return Ok(());
        }
        let _writing = self.writing.lock().map_err(|_| Error::Corrupt)?;
        if stamp < self.ceiling() {
            return Ok(());
        }
        let previous = load(self.device())?;
        let free = Free {
            ids: load_free(self.device(), &previous),
            runs: load_runs(self.device(), &previous),
        };
        let mut next = previous.clone();
        next.sequence = previous.sequence.saturating_add(1);
        next.stamp_ceiling = stamp.saturating_add(STAMP_CEILING_STEP);
        self.put(Some(&previous), next, &free)
    }
    fn put(
        &self,
        previous: Option<&Catalog>,
        mut recorded: Catalog,
        free: &Free,
    ) -> Result<(), Error> {
        recorded.stamp_ceiling =
            recorded.stamp_ceiling.max(previous.map_or(0, |c| c.stamp_ceiling)).max(self.ceiling());
        recorded.free_pages = store_free(self.device(), recorded.sequence, &free.ids)?;
        recorded.blob_free_pages =
            store_runs(self.device(), recorded.sequence, recorded.free_pages, &free.runs)?;
        store(self.device(), &recorded)?;
        self.ceiling.fetch_max(recorded.stamp_ceiling, core::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
}
