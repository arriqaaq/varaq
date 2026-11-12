use core::sync::atomic::{AtomicU32, Ordering};
#[derive(Debug)]
pub struct Collecting {
    running: AtomicU32,
}
impl Default for Collecting {
    fn default() -> Self {
        Self::new()
    }
}
impl Collecting {
    #[must_use]
    pub fn new() -> Self {
        Self { running: AtomicU32::new(0) }
    }
    pub fn enter(&self) -> Guard<'_> {
        self.running.fetch_add(1, Ordering::AcqRel);
        Guard { of: self }
    }
    #[must_use]
    pub fn any(&self) -> bool {
        self.running.load(Ordering::Acquire) > 0
    }
    #[must_use]
    pub fn count(&self) -> u32 {
        self.running.load(Ordering::Acquire)
    }
}
#[must_use = "the worker counts as collecting only while the guard lives"]
#[derive(Debug)]
pub struct Guard<'a> {
    of: &'a Collecting,
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.of.running.fetch_sub(1, Ordering::AcqRel);
    }
}
