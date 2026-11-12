#![allow(unsafe_code)]
use crate::page::checksum;
use crate::{Error, PAGE_SIZE, PageId};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
#[derive(Debug)]
pub struct MemoryStore {
    pages: Box<[AtomicPtr<u8>]>,
    allocated: AtomicUsize,
    limit: usize,
    barriers: crate::stats::Stat,
}
unsafe impl Send for MemoryStore {}
unsafe impl Sync for MemoryStore {}
impl Drop for MemoryStore {
    fn drop(&mut self) {
        for slot in &*self.pages {
            let p = slot.swap(core::ptr::null_mut(), Ordering::Acquire);
            if !p.is_null() {
                drop(unsafe { Box::from_raw(p.cast::<[u8; PAGE_SIZE]>()) });
            }
        }
    }
}
impl MemoryStore {
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            pages: (0..limit).map(|_| AtomicPtr::new(core::ptr::null_mut())).collect(),
            allocated: AtomicUsize::new(0),
            barriers: crate::stats::Stat::default(),
            limit,
        }
    }
    #[must_use]
    pub fn barriers(&self) -> u64 {
        self.barriers.get()
    }
    fn index_of(page: PageId) -> Option<usize> {
        usize::try_from(page.get()).ok()?.checked_sub(1)
    }
}
impl crate::store::PageStore for MemoryStore {
    fn read_raw(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        let Some(index) = Self::index_of(page) else {
            return Err(Error::NoSuchPage { page });
        };
        if index >= self.allocated.load(Ordering::Acquire) {
            return Err(Error::NoSuchPage { page });
        }
        let stored = self.pages[index].load(Ordering::Acquire);
        if stored.is_null() {
            into[..PAGE_SIZE].fill(0);
        } else {
            into[..PAGE_SIZE].copy_from_slice(unsafe { &*stored.cast::<[u8; PAGE_SIZE]>() });
        }
        Ok(())
    }
    fn read(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        self.read_raw(page, into)?;
        checksum::verify(&into[..PAGE_SIZE], page)
    }
    fn write(&self, page: PageId, from: &[u8]) -> Result<(), Error> {
        let Some(index) = Self::index_of(page) else {
            return Err(Error::NoSuchPage { page });
        };
        if index >= self.allocated.load(Ordering::Acquire) {
            return Err(Error::NoSuchPage { page });
        }
        let mut stored = self.pages[index].load(Ordering::Acquire);
        if stored.is_null() {
            let fresh = Box::into_raw(Box::new([0_u8; PAGE_SIZE])).cast::<u8>();
            match self.pages[index].compare_exchange(
                core::ptr::null_mut(),
                fresh,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => stored = fresh,
                Err(won) => {
                    drop(unsafe { Box::from_raw(fresh.cast::<[u8; PAGE_SIZE]>()) });
                    stored = won;
                }
            }
        }
        unsafe { &mut *stored.cast::<[u8; PAGE_SIZE]>() }.copy_from_slice(&from[..PAGE_SIZE]);
        Ok(())
    }
    fn allocate(&self) -> Result<PageId, Error> {
        self.allocate_run(1)
    }
    fn allocate_run(&self, pages: u64) -> Result<PageId, Error> {
        let pages = usize::try_from(pages).map_err(|_| Error::StoreFull { limit: self.limit })?;
        if pages == 0 {
            return Err(Error::Corrupt);
        }
        let mut n = self.allocated.load(Ordering::Acquire);
        loop {
            let end = n
                .checked_add(pages)
                .filter(|&end| end <= self.limit)
                .ok_or(Error::StoreFull { limit: self.limit })?;
            match self.allocated.compare_exchange_weak(n, end, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(seen) => n = seen,
            }
        }
        PageId::new(n as u64 + 1)
    }
    fn barrier(&self) -> Result<(), Error> {
        self.barriers.add(1);
        Ok(())
    }
    fn len(&self) -> usize {
        self.allocated.load(Ordering::Acquire).min(self.limit)
    }
}
