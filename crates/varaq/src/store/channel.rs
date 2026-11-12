use crate::{DeviceOp, Error, PageId, PageStore};
pub type PageBuf = Box<[u8]>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);
#[derive(Debug)]
pub struct Completion {
    pub id: RequestId,
    pub page: PageId,
    pub op: DeviceOp,
    pub buf: PageBuf,
    pub outcome: Result<(), Error>,
}
pub trait IoChannel: Send + Sync {
    fn push_read(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error>;
    fn push_write(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error>;
    fn submit(&self) -> Result<usize, Error>;
    fn poll(&self, min: usize) -> Result<Vec<Completion>, Error>;
    fn submitable(&self) -> usize;
    fn outstanding(&self) -> usize;
    fn barrier(&self) -> Result<(), Error>;
}
#[derive(Debug)]
pub struct BlockingChannel<S: PageStore> {
    inner: S,
    done: std::sync::Mutex<Vec<Completion>>,
    next: core::sync::atomic::AtomicU64,
}
impl<S: PageStore> BlockingChannel<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            done: std::sync::Mutex::new(Vec::new()),
            next: core::sync::atomic::AtomicU64::new(1),
        }
    }
    pub fn inner(&self) -> &S {
        &self.inner
    }
    fn id(&self) -> RequestId {
        RequestId(self.next.fetch_add(1, core::sync::atomic::Ordering::Relaxed))
    }
    fn finish(&self, c: Completion) -> Result<RequestId, Error> {
        let id = c.id;
        self.done.lock().map_err(|_| Error::Poisoned)?.push(c);
        Ok(id)
    }
}
impl<S: PageStore> IoChannel for BlockingChannel<S> {
    fn push_read(&self, page: PageId, mut buf: PageBuf) -> Result<RequestId, Error> {
        let outcome = self.inner.read(page, &mut buf);
        self.finish(Completion { id: self.id(), page, op: DeviceOp::Read, buf, outcome })
    }
    fn push_write(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        let outcome = self.inner.write(page, &buf);
        self.finish(Completion { id: self.id(), page, op: DeviceOp::Write, buf, outcome })
    }
    fn submit(&self) -> Result<usize, Error> {
        Ok(0)
    }
    fn poll(&self, _min: usize) -> Result<Vec<Completion>, Error> {
        Ok(core::mem::take(&mut *self.done.lock().map_err(|_| Error::Poisoned)?))
    }
    fn submitable(&self) -> usize {
        usize::MAX
    }
    fn outstanding(&self) -> usize {
        0
    }
    fn barrier(&self) -> Result<(), Error> {
        self.inner.barrier()
    }
}
#[derive(Debug, Default)]
pub struct Depth {
    submitted: core::sync::atomic::AtomicU64,
    completed: core::sync::atomic::AtomicU64,
    sum: core::sync::atomic::AtomicU64,
    samples: core::sync::atomic::AtomicU64,
    peak: core::sync::atomic::AtomicU64,
    failed: core::sync::atomic::AtomicU64,
    barriers: core::sync::atomic::AtomicU64,
}
impl Depth {
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        use core::sync::atomic::Ordering::Relaxed;
        let n = self.samples.load(Relaxed);
        if n == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.sum.load(Relaxed) as f64 / n as f64)
    }
    #[must_use]
    pub fn peak(&self) -> u64 {
        self.peak.load(core::sync::atomic::Ordering::Relaxed)
    }
    #[must_use]
    pub fn totals(&self) -> (u64, u64, u64) {
        use core::sync::atomic::Ordering::Relaxed;
        (self.submitted.load(Relaxed), self.completed.load(Relaxed), self.failed.load(Relaxed))
    }
    #[must_use]
    pub fn barriers(&self) -> u64 {
        self.barriers.load(core::sync::atomic::Ordering::Relaxed)
    }
}
#[derive(Debug)]
pub struct Counted<C: IoChannel> {
    inner: C,
    depth: Depth,
}
impl<C: IoChannel> Counted<C> {
    pub const fn new(inner: C) -> Self {
        Self { inner, depth: Depth::new_const() }
    }
    pub const fn depth(&self) -> &Depth {
        &self.depth
    }
    pub const fn inner(&self) -> &C {
        &self.inner
    }
}
impl Depth {
    const fn new_const() -> Self {
        use core::sync::atomic::AtomicU64;
        Self {
            submitted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            barriers: AtomicU64::new(0),
        }
    }
}
impl<C: IoChannel> IoChannel for Counted<C> {
    fn push_read(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        self.inner.push_read(page, buf)
    }
    fn push_write(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        self.inner.push_write(page, buf)
    }
    fn submit(&self) -> Result<usize, Error> {
        use core::sync::atomic::Ordering::Relaxed;
        let n = self.inner.submit()?;
        self.depth.submitted.fetch_add(n as u64, Relaxed);
        Ok(n)
    }
    fn poll(&self, min: usize) -> Result<Vec<Completion>, Error> {
        use core::sync::atomic::Ordering::Relaxed;
        let outstanding = self.inner.outstanding() as u64;
        self.depth.sum.fetch_add(outstanding, Relaxed);
        self.depth.samples.fetch_add(1, Relaxed);
        self.depth.peak.fetch_max(outstanding, Relaxed);
        let done = self.inner.poll(min)?;
        self.depth.completed.fetch_add(done.len() as u64, Relaxed);
        self.depth
            .failed
            .fetch_add(done.iter().filter(|c| c.outcome.is_err()).count() as u64, Relaxed);
        Ok(done)
    }
    fn submitable(&self) -> usize {
        self.inner.submitable()
    }
    fn outstanding(&self) -> usize {
        self.inner.outstanding()
    }
    fn barrier(&self) -> Result<(), Error> {
        self.depth.barriers.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.inner.barrier()
    }
}
