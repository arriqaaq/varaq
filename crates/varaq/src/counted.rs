#![allow(unsafe_code)]
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub allocations: u64,
    pub bytes: u64,
    pub leaked: i64,
}
impl Cost {
    #[must_use]
    pub fn is_free(&self) -> bool {
        self.allocations == 0 && self.bytes == 0
    }
}
thread_local! {
    static ALLOCATIONS : Cell < u64 > = const { Cell::new(0) }; static BYTES : Cell < u64
    > = const { Cell::new(0) }; static LIVE : Cell < i64 > = const { Cell::new(0) };
}
fn bump<T: Copy + core::ops::Add<Output = T> + 'static>(
    key: &'static std::thread::LocalKey<Cell<T>>,
    by: T,
) {
    let _ = key.try_with(|c| c.set(c.get() + by));
}
fn read<T: Copy + Default + 'static>(key: &'static std::thread::LocalKey<Cell<T>>) -> T {
    key.try_with(Cell::get).unwrap_or_default()
}
pub struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(&ALLOCATIONS, 1);
        bump(&BYTES, layout.size() as u64);
        bump(&LIVE, 1);
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(&LIVE, -1);
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(&ALLOCATIONS, 1);
        bump(&BYTES, new_size as u64);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}
pub fn allocations_during<R>(f: impl FnOnce() -> R) -> (R, Cost) {
    let a0 = read(&ALLOCATIONS);
    let b0 = read(&BYTES);
    let l0 = read(&LIVE);
    let out = f();
    let cost = Cost {
        allocations: read(&ALLOCATIONS) - a0,
        bytes: read(&BYTES) - b0,
        leaked: read(&LIVE) - l0,
    };
    (out, cost)
}
#[must_use]
pub fn totals() -> Cost {
    Cost { allocations: read(&ALLOCATIONS), bytes: read(&BYTES), leaked: read(&LIVE) }
}
