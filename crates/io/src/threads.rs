use core::sync::atomic::Ordering;
use core::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use waraq::store::channel::{Completion, IoChannel, PageBuf, RequestId};
use waraq::{DeviceOp, Error, PageId, PageStore};
enum Request {
    Call { id: RequestId, page: PageId, op: DeviceOp, buf: PageBuf },
    Stop,
}
pub struct ThreadChannel<S: PageStore + Send + Sync + 'static> {
    store: Arc<S>,
    requests: Mutex<Option<mpsc::Sender<Request>>>,
    done: Mutex<mpsc::Receiver<Completion>>,
    readers: Vec<thread::JoinHandle<()>>,
    next: AtomicU64,
    outstanding: AtomicUsize,
    capacity: usize,
}
impl<S: PageStore + Send + Sync + 'static> ThreadChannel<S> {
    #[must_use]
    pub fn new(store: S, readers: usize, capacity: usize) -> Self {
        assert!(
            readers > 0 && capacity > 0,
            "a thread channel needs a reader and room for a request"
        );
        let store = Arc::new(store);
        let (send, receive) = mpsc::channel::<Request>();
        let (finished, done) = mpsc::channel::<Completion>();
        let receive = Arc::new(Mutex::new(receive));
        let readers = (0..readers)
            .map(|_| {
                let (store, receive, finished) =
                    (Arc::clone(&store), Arc::clone(&receive), finished.clone());
                thread::spawn(move || serve(&*store, &receive, &finished))
            })
            .collect();
        Self {
            store,
            requests: Mutex::new(Some(send)),
            done: Mutex::new(done),
            readers,
            next: AtomicU64::new(1),
            outstanding: AtomicUsize::new(0),
            capacity,
        }
    }
    pub fn inner(&self) -> &S {
        &self.store
    }
    fn push(&self, page: PageId, op: DeviceOp, buf: PageBuf) -> Result<RequestId, Error> {
        if self.outstanding.load(Ordering::Acquire) >= self.capacity {
            return Err(Error::Full { capacity: self.capacity });
        }
        let id = RequestId(self.next.fetch_add(1, Ordering::Relaxed));
        let requests = self.requests.lock().map_err(|_| Error::Poisoned)?;
        let sender = requests.as_ref().ok_or(Error::Device { page: Some(page), op })?;
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        if sender.send(Request::Call { id, page, op, buf }).is_err() {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
            return Err(Error::Device { page: Some(page), op });
        }
        Ok(id)
    }
}
fn serve<S: PageStore>(
    store: &S,
    receive: &Mutex<mpsc::Receiver<Request>>,
    finished: &mpsc::Sender<Completion>,
) {
    loop {
        let request = match receive.lock() {
            Ok(queue) => queue.recv(),
            Err(_) => return,
        };
        let Ok(Request::Call { id, page, op, mut buf }) = request else { return };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match op {
            DeviceOp::Read => store.read(page, &mut buf),
            DeviceOp::Write => store.write(page, &buf),
        }))
        .unwrap_or(Err(Error::Device { page: Some(page), op }));
        if finished.send(Completion { id, page, op, buf, outcome }).is_err() {
            return;
        }
    }
}
impl<S: PageStore + Send + Sync + 'static> IoChannel for ThreadChannel<S> {
    fn push_read(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        self.push(page, DeviceOp::Read, buf)
    }
    fn push_write(&self, page: PageId, buf: PageBuf) -> Result<RequestId, Error> {
        self.push(page, DeviceOp::Write, buf)
    }
    fn submit(&self) -> Result<usize, Error> {
        Ok(0)
    }
    fn poll(&self, min: usize) -> Result<Vec<Completion>, Error> {
        let done = self.done.lock().map_err(|_| Error::Poisoned)?;
        let want = min.min(self.outstanding.load(Ordering::Acquire));
        let mut out = Vec::new();
        while let Ok(c) = done.try_recv() {
            out.push(c);
        }
        while out.len() < want {
            match done.recv() {
                Ok(c) => out.push(c),
                Err(_) => {
                    return Err(Error::Device { page: None, op: DeviceOp::Read });
                }
            }
        }
        self.outstanding.fetch_sub(out.len(), Ordering::AcqRel);
        Ok(out)
    }
    fn submitable(&self) -> usize {
        self.capacity.saturating_sub(self.outstanding.load(Ordering::Acquire))
    }
    fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }
    fn barrier(&self) -> Result<(), Error> {
        self.store.barrier()
    }
}
impl<S: PageStore + Send + Sync + 'static> Drop for ThreadChannel<S> {
    fn drop(&mut self) {
        if let Ok(mut requests) = self.requests.lock()
            && let Some(sender) = requests.take()
        {
            for _ in 0..self.readers.len() {
                let _ = sender.send(Request::Stop);
            }
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}
impl<S: PageStore + Send + Sync + 'static> core::fmt::Debug for ThreadChannel<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThreadChannel")
            .field("readers", &self.readers.len())
            .field("capacity", &self.capacity)
            .field("outstanding", &self.outstanding.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}
