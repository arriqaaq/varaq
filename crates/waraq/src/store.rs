use crate::{Error, PageId};
pub trait PageStore: Send + Sync {
    fn read(&self, page: PageId, into: &mut [u8]) -> Result<(), Error>;
    fn read_raw(&self, page: PageId, into: &mut [u8]) -> Result<(), Error>;
    fn read_raw_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        if !into.len().is_multiple_of(crate::PAGE_SIZE) {
            return Err(Error::Corrupt);
        }
        for (i, page) in into.as_chunks_mut::<{ crate::PAGE_SIZE }>().0.iter_mut().enumerate() {
            self.read_raw(PageId::new(first.get() + i as u64)?, page)?;
        }
        Ok(())
    }
    fn read_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        if !into.len().is_multiple_of(crate::PAGE_SIZE) {
            return Err(Error::Corrupt);
        }
        for (i, page) in into.as_chunks_mut::<{ crate::PAGE_SIZE }>().0.iter_mut().enumerate() {
            self.read(PageId::new(first.get() + i as u64)?, page)?;
        }
        Ok(())
    }
    fn write(&self, page: PageId, from: &[u8]) -> Result<(), Error>;
    fn write_run(&self, first: PageId, from: &[u8]) -> Result<(), Error> {
        if !from.len().is_multiple_of(crate::PAGE_SIZE) {
            return Err(Error::Corrupt);
        }
        for (i, page) in from.as_chunks::<{ crate::PAGE_SIZE }>().0.iter().enumerate() {
            self.write(PageId::new(first.get() + i as u64)?, page)?;
        }
        Ok(())
    }
    fn allocate(&self) -> Result<PageId, Error>;
    fn allocate_run(&self, pages: u64) -> Result<PageId, Error> {
        if pages == 0 {
            return Err(Error::Corrupt);
        }
        let first = self.allocate()?;
        for i in 1..pages {
            if self.allocate()?.get() != first.get() + i {
                return Err(Error::Corrupt);
            }
        }
        Ok(first)
    }
    fn resume_issued(&self, issued: u64) -> Result<(), Error> {
        while (self.len() as u64) < issued {
            self.allocate()?;
        }
        Ok(())
    }
    fn barrier(&self) -> Result<(), Error>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
impl<S: PageStore + ?Sized> PageStore for &S {
    fn read_raw(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read_raw(page, into)
    }
    fn read_raw_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read_raw_run(first, into)
    }
    fn read(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read(page, into)
    }
    fn read_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read_run(first, into)
    }
    fn write(&self, page: PageId, from: &[u8]) -> Result<(), Error> {
        (**self).write(page, from)
    }
    fn write_run(&self, first: PageId, from: &[u8]) -> Result<(), Error> {
        (**self).write_run(first, from)
    }
    fn allocate(&self) -> Result<PageId, Error> {
        (**self).allocate()
    }
    fn allocate_run(&self, pages: u64) -> Result<PageId, Error> {
        (**self).allocate_run(pages)
    }
    fn resume_issued(&self, issued: u64) -> Result<(), Error> {
        (**self).resume_issued(issued)
    }
    fn barrier(&self) -> Result<(), Error> {
        (**self).barrier()
    }
    fn len(&self) -> usize {
        (**self).len()
    }
}
impl<S: PageStore + ?Sized> PageStore for std::sync::Arc<S> {
    fn read_raw(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read_raw(page, into)
    }
    fn read_raw_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read_raw_run(first, into)
    }
    fn read(&self, page: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read(page, into)
    }
    fn read_run(&self, first: PageId, into: &mut [u8]) -> Result<(), Error> {
        (**self).read_run(first, into)
    }
    fn write(&self, page: PageId, from: &[u8]) -> Result<(), Error> {
        (**self).write(page, from)
    }
    fn write_run(&self, first: PageId, from: &[u8]) -> Result<(), Error> {
        (**self).write_run(first, from)
    }
    fn allocate(&self) -> Result<PageId, Error> {
        (**self).allocate()
    }
    fn allocate_run(&self, pages: u64) -> Result<PageId, Error> {
        (**self).allocate_run(pages)
    }
    fn resume_issued(&self, issued: u64) -> Result<(), Error> {
        (**self).resume_issued(issued)
    }
    fn barrier(&self) -> Result<(), Error> {
        (**self).barrier()
    }
    fn len(&self) -> usize {
        (**self).len()
    }
}
pub mod channel;
pub mod memory;
pub use channel::{BlockingChannel, Completion, IoChannel, PageBuf, RequestId};
pub use memory::MemoryStore;
