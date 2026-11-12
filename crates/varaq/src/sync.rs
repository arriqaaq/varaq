pub use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct Padded<T>(pub T);
impl<T> core::ops::Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
