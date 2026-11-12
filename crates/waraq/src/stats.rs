#[derive(Default)]
pub struct Stat(imp::Stat);
impl Stat {
    #[inline]
    pub fn add(&self, n: u64) {
        self.0.add(n);
    }
    #[inline]
    pub fn sub(&self, n: u64) {
        self.0.sub(n);
    }
    #[inline]
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.get()
    }
}
impl core::fmt::Debug for Stat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Stat").field(&self.get()).finish()
    }
}
pub const KEPT: bool = cfg!(feature = "stats");
#[cfg(feature = "stats")]
mod imp {
    use core::sync::atomic::{AtomicU64, Ordering};
    const STRIPES: usize = 16;
    #[derive(Default)]
    pub(super) struct Stat(Box<[Stripe; STRIPES]>);
    #[derive(Default)]
    #[repr(align(128))]
    struct Stripe(AtomicU64);
    fn stripe() -> usize {
        static NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        thread_local! {
            static STRIPE : usize = NEXT.fetch_add(1, Ordering::Relaxed) % STRIPES;
        }
        STRIPE.with(|s| *s)
    }
    impl Stat {
        #[inline]
        pub(super) fn add(&self, n: u64) {
            self.0[stripe()].0.fetch_add(n, Ordering::Relaxed);
        }
        #[inline]
        pub(super) fn sub(&self, n: u64) {
            self.0[stripe()].0.fetch_sub(n, Ordering::Relaxed);
        }
        pub(super) fn get(&self) -> u64 {
            self.0.iter().fold(0_u64, |sum, s| sum.wrapping_add(s.0.load(Ordering::Relaxed)))
        }
    }
}
#[cfg(not(feature = "stats"))]
mod imp {
    #[derive(Default)]
    pub(super) struct Stat;
    #[expect(
        clippy::unused_self,
        reason = "the same methods as the counting form, so a call site does not know which it has"
    )]
    impl Stat {
        #[inline]
        pub(super) fn add(&self, _n: u64) {}
        #[inline]
        pub(super) fn sub(&self, _n: u64) {}
        #[inline]
        pub(super) fn get(&self) -> u64 {
            0
        }
    }
    const _: () = assert!(core::mem::size_of::<super::Stat>() == 0);
}
