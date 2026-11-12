#![allow(unsafe_code)]
use io_uring::{IoUring, opcode, types};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use waraq::store::channel::{Completion, IoChannel, PageBuf, RequestId};
use waraq::{DeviceOp, Error, PAGE_SIZE, PageId};
#[derive(Debug)]
struct Slot {
    id: RequestId,
    page: PageId,
    op: DeviceOp,
    buf: PageBuf,
}
struct Slab {
    slots: Vec<Option<Slot>>,
    pages: Vec<Box<crate::AlignedPage>>,
}
pub struct UringChannel {
    file: File,
    ring: Mutex<IoUring>,
    slab: Mutex<Slab>,
    staged: Mutex<usize>,
    inflight: Mutex<usize>,
    next_id: Mutex<u64>,
    depth: usize,
    direct: bool,
}
impl core::fmt::Debug for UringChannel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UringChannel")
            .field("depth", &self.depth)
            .field("direct", &self.direct)
            .field("outstanding", &self.outstanding())
            .finish_non_exhaustive()
    }
}
impl UringChannel {
    pub fn open(path: &Path, depth: usize) -> Result<Self, Error> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| Error::Device { page: None, op: DeviceOp::Read })?;
        Self::over(file, depth, false)
    }
    pub fn open_direct(path: &Path, depth: usize) -> Result<Option<Self>, Error> {
        let Some(file) = crate::platform::open_direct(path)
            .map_err(|_| Error::Device { page: None, op: DeviceOp::Read })?
        else {
            return Ok(None);
        };
        Self::over(file, depth, true).map(Some)
    }
    fn over(file: File, depth: usize, direct: bool) -> Result<Self, Error> {
        let entries = u32::try_from(depth.next_power_of_two())
            .map_err(|_| Error::Device { page: None, op: DeviceOp::Read })?;
        let ring =
            IoUring::new(entries).map_err(|_| Error::Device { page: None, op: DeviceOp::Read })?;
        let mut slots = Vec::new();
        slots.resize_with(depth, || None);
        let pages = if direct {
            (0..depth).map(|_| Box::new(crate::AlignedPage([0; PAGE_SIZE]))).collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            file,
            ring: Mutex::new(ring),
            slab: Mutex::new(Slab { slots, pages }),
            staged: Mutex::new(0),
            inflight: Mutex::new(0),
            next_id: Mutex::new(1),
            depth,
            direct,
        })
    }
    fn offset_of(page: PageId) -> u64 {
        (page.get() - 1) * PAGE_SIZE as u64
    }
    fn push(&self, page: PageId, mut buf: PageBuf, op: DeviceOp) -> Result<RequestId, Error> {
        let mut slab = self.slab.lock().map_err(|_| Error::Poisoned)?;
        let Some(index) = slab.slots.iter().position(Option::is_none) else {
            return Err(Error::Full { capacity: self.depth });
        };
        if self.direct && buf.len() != PAGE_SIZE {
            return Err(Error::Device { page: Some(page), op });
        }
        let id = {
            let mut next = self.next_id.lock().map_err(|_| Error::Poisoned)?;
            let id = RequestId(*next);
            *next += 1;
            id
        };
        let (ptr, len) = if self.direct {
            let aligned = slab.pages.get_mut(index).ok_or(Error::Corrupt)?;
            if op == DeviceOp::Write {
                aligned.0.copy_from_slice(&buf);
            }
            (aligned.0.as_mut_ptr(), PAGE_SIZE)
        } else {
            (buf.as_mut_ptr(), buf.len())
        };
        let len = u32::try_from(len).map_err(|_| Error::Corrupt)?;
        slab.slots[index] = Some(Slot { id, page, op, buf });
        let offset = Self::offset_of(page);
        let fd = types::Fd(self.file.as_raw_fd());
        let entry = match op {
            DeviceOp::Read => opcode::Read::new(fd, ptr, len).offset(offset).build(),
            DeviceOp::Write => opcode::Write::new(fd, ptr, len).offset(offset).build(),
        }
        .user_data(index as u64);
        let mut ring = self.ring.lock().map_err(|_| Error::Poisoned)?;
        let pushed = unsafe { ring.submission().push(&entry) };
        if pushed.is_err() {
            drop(ring);
            slab.slots[index] = None;
            return Err(Error::Full { capacity: self.depth });
        }
        drop(ring);
        *self.staged.lock().map_err(|_| Error::Poisoned)? += 1;
        Ok(id)
    }
}
impl IoChannel for UringChannel {
    fn push_read(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        self.push(page, buf, DeviceOp::Read)
    }
    fn push_write(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        self.push(page, buf, DeviceOp::Write)
    }
    fn submit(&self) -> Result<usize, Error> {
        let mut staged = self.staged.lock().map_err(|_| Error::Poisoned)?;
        if *staged == 0 {
            return Ok(0);
        }
        let ring = self.ring.lock().map_err(|_| Error::Poisoned)?;
        let taken = ring.submit().map_err(|_| Error::Device { page: None, op: DeviceOp::Write })?;
        drop(ring);
        let n = taken.min(*staged);
        *staged -= n;
        *self.inflight.lock().map_err(|_| Error::Poisoned)? += n;
        Ok(n)
    }
    fn poll(&self, min: usize) -> Result<Vec<Completion>, Error> {
        let mut ring = self.ring.lock().map_err(|_| Error::Poisoned)?;
        let handed = if min > 0 {
            ring.submit_and_wait(min)
                .map_err(|_| Error::Device { page: None, op: DeviceOp::Read })?
        } else {
            0
        };
        let mut reaped = Vec::new();
        for cqe in ring.completion() {
            reaped.push((cqe.user_data(), cqe.result()));
        }
        drop(ring);
        let mut slab = self.slab.lock().map_err(|_| Error::Poisoned)?;
        let mut done = Vec::with_capacity(reaped.len());
        for (user_data, result) in reaped {
            let Ok(index) = usize::try_from(user_data) else { continue };
            let Some(mut entry) = slab.slots.get_mut(index).and_then(Option::take) else {
                continue;
            };
            let whole = usize::try_from(result).is_ok_and(|n| n == entry.buf.len());
            let outcome = if whole {
                Ok(())
            } else {
                Err(Error::Device { page: Some(entry.page), op: entry.op })
            };
            if whole
                && self.direct
                && entry.op == DeviceOp::Read
                && let Some(aligned) = slab.pages.get(index)
            {
                entry.buf.copy_from_slice(&aligned.0);
            }
            done.push(Completion {
                id: entry.id,
                page: entry.page,
                op: entry.op,
                buf: entry.buf,
                outcome,
            });
        }
        drop(slab);
        let handed = {
            let mut staged = self.staged.lock().map_err(|_| Error::Poisoned)?;
            let n = handed.min(*staged);
            *staged -= n;
            n
        };
        let mut inflight = self.inflight.lock().map_err(|_| Error::Poisoned)?;
        *inflight = (*inflight + handed).saturating_sub(done.len());
        Ok(done)
    }
    fn submitable(&self) -> usize {
        self.slab.lock().map_or(0, |s| s.slots.iter().filter(|x| x.is_none()).count())
    }
    fn outstanding(&self) -> usize {
        *self.inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn barrier(&self) -> Result<(), Error> {
        self.file.sync_data().map_err(|_| Error::Device { page: None, op: DeviceOp::Write })
    }
}
const CANCEL: u64 = u64::MAX;
impl UringChannel {
    fn settle(&mut self) -> std::io::Result<()> {
        let ring = self.ring.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        let slab = self.slab.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        let held: Vec<usize> =
            slab.slots.iter().enumerate().filter(|(_, s)| s.is_some()).map(|(i, _)| i).collect();
        if held.is_empty() {
            return Ok(());
        }
        while !ring.submission().is_empty() {
            if ring.submit()? == 0 {
                return Err(std::io::Error::other("the kernel took nothing of a full queue"));
            }
        }
        for &index in &held {
            let cancel = opcode::AsyncCancel::new(index as u64).build().user_data(CANCEL);
            if unsafe { ring.submission().push(&cancel) }.is_err() {
                return Err(std::io::Error::other("no room for a cancellation"));
            }
        }
        let mut left = held.len();
        while left > 0 {
            match ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
            for cqe in ring.completion() {
                let reaped = usize::try_from(cqe.user_data())
                    .ok()
                    .and_then(|index| slab.slots.get_mut(index))
                    .and_then(Option::take);
                if reaped.is_some() {
                    left -= 1;
                }
            }
        }
        Ok(())
    }
}
impl Drop for UringChannel {
    fn drop(&mut self) {
        if self.settle().is_err() {
            let slab = self.slab.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::forget(std::mem::take(&mut slab.slots));
            std::mem::forget(std::mem::take(&mut slab.pages));
        }
    }
}
