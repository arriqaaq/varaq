use crate::sync::Mutex;
use core::sync::atomic::{AtomicUsize, Ordering};
#[derive(Debug)]
#[repr(align(64))]
pub struct FreeList {
    queue: Mutex<Vec<u32>>,
    count: AtomicUsize,
}
impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}
impl FreeList {
    #[must_use]
    pub fn new() -> Self {
        Self { queue: Mutex::new(Vec::new()), count: AtomicUsize::new(0) }
    }
    #[must_use]
    pub fn preloaded(indices: Vec<u32>) -> Self {
        let n = indices.len();
        let this = Self { queue: Mutex::new(indices), count: AtomicUsize::new(n) };
        drop(this.queue.lock());
        this
    }
    #[must_use]
    pub fn with_capacity(frames: usize) -> Self {
        Self { queue: Mutex::new(Vec::with_capacity(2 * frames + 16)), count: AtomicUsize::new(0) }
    }
    pub fn push(&self, index: u32) {
        self.push_compacting(index, |_| true);
    }
    pub fn push_compacting(&self, index: u32, is_free: impl Fn(u32) -> bool) {
        let Ok(mut queue) = self.queue.lock() else { return };
        if queue.capacity() > 0 && queue.len() >= queue.capacity() {
            queue.retain(|&i| is_free(i));
            if 2 * queue.len() >= queue.capacity() {
                queue.sort_unstable();
                queue.dedup();
            }
        }
        queue.push(index);
        self.count.store(queue.len(), Ordering::Relaxed);
    }
    pub fn pop(&self) -> Option<u32> {
        if self.count.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let Ok(mut queue) = self.queue.lock() else { return None };
        let index = queue.pop()?;
        self.count.store(queue.len(), Ordering::Relaxed);
        Some(index)
    }
    pub(crate) fn pop_and_claim(&self, mut claim: impl FnMut(u32) -> bool) -> Option<u32> {
        while let Some(index) = self.pop() {
            if claim(index) {
                return Some(index);
            }
        }
        None
    }
    #[must_use]
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.lock().map_or(0, |q| q.len())
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }
}
