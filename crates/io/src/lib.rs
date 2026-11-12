pub mod devices;
#[cfg(all(feature = "opfs", target_arch = "wasm32"))]
pub mod opfs;
#[cfg(unix)]
mod platform;
mod positional;
#[cfg(all(target_os = "linux", feature = "uring"))]
pub(crate) use positional::AlignedPage;
#[cfg(unix)]
pub use positional::DirectFile;
pub use positional::PositionalFile;
use positional::{read_whole, write_whole};
pub mod threads;
#[cfg(all(target_os = "linux", feature = "uring"))]
pub mod uring;
#[cfg(unix)]
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use varaq::page::checksum;
use varaq::{DeviceOp, Error, PAGE_SIZE, PageId, PageStore};
#[derive(Debug)]
pub struct FileStore<F = std::fs::File> {
    file: F,
    allocated: AtomicU64,
    grown: varaq::sync::Mutex<u64>,
    reserved: AtomicU64,
}
pub const GROWTH_PAGES: u64 = 64;
pub const GROWTH_FRACTION: u64 = 8;
#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), Error> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|_| Error::Device { page: None, op: DeviceOp::Write })
}
#[cfg(unix)]
impl FileStore<std::fs::File> {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let existed = path.exists();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| store_failed(DeviceOp::Read))?;
        if !existed {
            sync_parent(path)?;
        }
        Self::adopt(file)
    }
    pub fn open_direct(path: &Path) -> Result<Option<FileStore<DirectFile>>, Error> {
        let existed = path.exists();
        let Some(file) = platform::open_direct(path).map_err(|_| store_failed(DeviceOp::Read))?
        else {
            return Ok(None);
        };
        if !existed {
            sync_parent(path)?;
        }
        FileStore::adopt(DirectFile(file)).map(Some)
    }
    #[must_use]
    pub const fn direct_alignment() -> usize {
        platform::direct_alignment()
    }
    pub fn advise_no_cache(&self) -> Result<bool, Error> {
        platform::advise_no_cache(&self.file).map_err(|()| store_failed(DeviceOp::Read))
    }
}
pub(crate) fn store_failed(op: DeviceOp) -> Error {
    Error::Device { page: None, op }
}
impl<F: PositionalFile> FileStore<F> {
    pub fn adopt(file: F) -> Result<Self, Error> {
        let len = file.size().map_err(|_| store_failed(DeviceOp::Read))?;
        Ok(Self {
            file,
            allocated: AtomicU64::new(len / PAGE_SIZE as u64),
            grown: varaq::sync::Mutex::new(len),
            reserved: AtomicU64::new(len),
        })
    }
    pub fn sync(&self) -> Result<(), Error> {
        self.file.sync_data().map_err(|_| store_failed(DeviceOp::Write))
    }
    fn offset_of(page: PageId) -> Option<u64> {
        page.get().checked_sub(1)?.checked_mul(PAGE_SIZE as u64)
    }
    fn in_range(&self, page: PageId) -> bool {
        page.get() <= self.allocated.load(Ordering::Acquire)
    }
    #[inline]
    fn extent(&self, first: PageId, bytes: usize) -> Result<u64, Error> {
        let Some(at) = Self::offset_of(first) else {
            return Err(Error::NoSuchPage { page: first });
        };
        let last = if bytes <= PAGE_SIZE {
            first
        } else {
            PageId::new(first.get() + (bytes / PAGE_SIZE) as u64 - 1)?
        };
        if !self.in_range(last) {
            return Err(Error::NoSuchPage { page: last });
        }
        Ok(at)
    }
    fn page_at(first: PageId, at: u64, stopped: u64) -> PageId {
        PageId::new(first.get() + (stopped.saturating_sub(at)) / PAGE_SIZE as u64).unwrap_or(first)
    }
    #[inline]
    fn read_extent(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        let at = self.extent(first, into.len())?;
        read_whole(&self.file, into, at)
            .map_err(|stopped| device(Self::page_at(first, at, stopped), DeviceOp::Read))
    }
    fn grow(&self, grown: &mut u64, end: u64, pages: u64, page: PageId) -> Result<(), Error> {
        let full = || Error::StoreFull { limit: usize::try_from(page.get()).unwrap_or(usize::MAX) };
        let run = GROWTH_PAGES * PAGE_SIZE as u64;
        let (least, to) =
            calc::growth_target(*grown, end, run, GROWTH_FRACTION, pages == 1).ok_or_else(full)?;
        let room = |e: &std::io::Error| {
            matches!(e.kind(), std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded)
        };
        let extend = |len| {
            if pages == 1 { self.file.reserve(len) } else { self.file.set_len(len) }
        };
        let reached = match extend(to) {
            Ok(()) => to,
            Err(e) if room(&e) && to > least => match extend(least) {
                Ok(()) => least,
                Err(e) if room(&e) => return Err(full()),
                Err(_) => return Err(device(page, DeviceOp::Write)),
            },
            Err(e) if room(&e) => return Err(full()),
            Err(_) => return Err(device(page, DeviceOp::Write)),
        };
        *grown = reached;
        self.reserved.store(reached, Ordering::Release);
        Ok(())
    }
    #[inline]
    fn write_extent(&self, first: PageId, from: &[u8]) -> Result<(), Error> {
        let at = self.extent(first, from.len())?;
        write_whole(&self.file, from, at)
            .map_err(|stopped| device(Self::page_at(first, at, stopped), DeviceOp::Write))
    }
}
fn device(page: PageId, op: DeviceOp) -> Error {
    Error::Device { page: Some(page), op }
}
impl<F: PositionalFile> PageStore for FileStore<F> {
    fn read_raw(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        self.read_extent(page, &mut into[..PAGE_SIZE])
    }
    fn read_raw_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        if !into.len().is_multiple_of(PAGE_SIZE) {
            return Err(Error::Corrupt);
        }
        if into.is_empty() {
            return Ok(());
        }
        self.read_extent(first, into)
    }
    fn read(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        self.read_raw(page, into)?;
        checksum::verify(&into[..PAGE_SIZE], page)
    }
    fn read_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        if !into.len().is_multiple_of(PAGE_SIZE) {
            return Err(Error::Corrupt);
        }
        if into.is_empty() {
            return Ok(());
        }
        self.read_extent(first, into)?;
        for (i, page) in into.as_chunks::<PAGE_SIZE>().0.iter().enumerate() {
            checksum::verify(page, PageId::new(first.get() + i as u64)?)?;
        }
        Ok(())
    }
    fn write(&self, page: PageId, from: &[u8]) -> Result<(), Error> {
        self.write_extent(page, &from[..PAGE_SIZE])
    }
    fn write_run(&self, first: PageId, from: &[u8]) -> Result<(), Error> {
        if !from.len().is_multiple_of(PAGE_SIZE) {
            return Err(Error::Corrupt);
        }
        if from.is_empty() {
            return Ok(());
        }
        self.write_extent(first, from)
    }
    fn allocate(&self) -> Result<PageId, Error> {
        self.allocate_run(1)
    }
    fn allocate_run(&self, pages: u64) -> Result<PageId, Error> {
        if pages == 0 {
            return Err(Error::Corrupt);
        }
        let n = self.allocated.fetch_add(pages, Ordering::AcqRel) + 1;
        let full = || Error::StoreFull { limit: usize::try_from(n).unwrap_or(usize::MAX) };
        let page = PageId::new(n).map_err(|_| full())?;
        let last = PageId::new(n.checked_add(pages - 1).ok_or_else(full)?).map_err(|_| full())?;
        let end = Self::offset_of(last)
            .and_then(|at| at.checked_add(PAGE_SIZE as u64))
            .ok_or_else(full)?;
        let reserved = self.reserved.load(Ordering::Acquire);
        if end <= reserved {
            let step = reserved / GROWTH_FRACTION;
            if pages == 1
                && step > GROWTH_PAGES * PAGE_SIZE as u64
                && reserved - end < step / 2
                && let Ok(mut grown) = self.grown.try_lock()
                && *grown == reserved
            {
                let _ = self.grow(&mut grown, end, pages, page);
            }
            return Ok(page);
        }
        let mut grown = self.grown.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if end > *grown {
            self.grow(&mut grown, end, pages, page)?;
        }
        Ok(page)
    }
    fn resume_issued(&self, issued: u64) -> Result<(), Error> {
        let held = self.allocated.load(Ordering::Acquire);
        if issued <= held {
            self.allocated.store(issued, Ordering::Release);
            return Ok(());
        }
        while (self.len() as u64) < issued {
            self.allocate()?;
        }
        Ok(())
    }
    fn barrier(&self) -> Result<(), Error> {
        self.sync()
    }
    fn len(&self) -> usize {
        usize::try_from(self.allocated.load(Ordering::Acquire)).unwrap_or(usize::MAX)
    }
}
