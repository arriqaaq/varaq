#![allow(unsafe_code)]
use crate::{Optimistic, Unwind};
use core::marker::PhantomData;
use core::mem::{MaybeUninit, size_of};
use core::sync::atomic::{AtomicU64, Ordering, fence};
const STATE_SHIFT: u32 = 56;
const STATE: u64 = 0xFF << STATE_SHIFT;
const EXCLUSIVE: u64 = STATE;
const ONE_SHARER: u64 = 1 << STATE_SHIFT;
const SHARED_MAX: u64 = 254;
const WAITING_SHIFT: u32 = 48;
const WAITING: u64 = 0xFF << WAITING_SHIFT;
const ONE_WAITER: u64 = 1 << WAITING_SHIFT;
const WAITING_MAX: u64 = 0xFF;
const VERSION: u64 = ONE_WAITER - 1;
const HELD: Ordering = Ordering::Relaxed;
const fn exclusive_in(word: u64) -> bool {
    word & STATE == EXCLUSIVE
}
const fn released(word: u64) -> u64 {
    let next = (word & VERSION) + 1;
    (word & WAITING) | if next >= VERSION { 0 } else { next }
}
pub unsafe trait AnyBits: Copy {}
unsafe impl AnyBits for u8 {}
unsafe impl AnyBits for u16 {}
unsafe impl AnyBits for u32 {}
unsafe impl AnyBits for u64 {}
unsafe impl AnyBits for usize {}
unsafe impl<T: AnyBits, const N: usize> AnyBits for [T; N] {}
unsafe impl AnyBits for crate::child::ChildRef {}
pub const SPIN_BUDGET: u32 = 64;
#[inline]
pub(crate) fn spin() {
    core::hint::spin_loop();
}
#[inline]
pub(crate) fn give_back() {
    spin();
    std::thread::yield_now();
}
pub struct HybridLatch<T: AnyBits, const W: usize> {
    version: AtomicU64,
    words: [AtomicU64; W],
    _value: PhantomData<T>,
}
impl<T: AnyBits, const W: usize> HybridLatch<T, W> {
    #[must_use]
    pub fn new(value: T) -> Self {
        const { assert!(W * 8 >= size_of::<T>()) };
        let src = to_words::<T, W>(&value);
        Self {
            version: AtomicU64::new(0),
            words: core::array::from_fn(|i| AtomicU64::new(src[i])),
            _value: PhantomData,
        }
    }
    fn load(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
    pub fn optimistic(&self) -> Optimistic<OptimisticGuard<'_, T, W>> {
        let word = self.load();
        if exclusive_in(word) {
            return Err(Unwind);
        }
        Ok(OptimisticGuard { latch: self, version: word & VERSION })
    }
    pub fn optimistic_or_spin(&self) -> Optimistic<OptimisticGuard<'_, T, W>> {
        for _ in 0..SPIN_BUDGET {
            if let Ok(guard) = self.optimistic() {
                return Ok(guard);
            }
            spin();
        }
        Err(Unwind)
    }
    #[must_use]
    pub fn announce(&self) -> Option<Waiting<'_>> {
        let mut word = self.version.load(Ordering::Relaxed);
        loop {
            if (word & WAITING) >> WAITING_SHIFT == WAITING_MAX {
                return None;
            }
            match self.version.compare_exchange_weak(
                word,
                word + ONE_WAITER,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Waiting { word: &self.version });
                }
                Err(now) => word = now,
            }
        }
    }
    #[must_use]
    pub fn is_announced_by(&self, waiting: &Waiting<'_>) -> bool {
        core::ptr::eq(waiting.word, core::ptr::from_ref(&self.version))
    }
    #[must_use]
    pub fn waiting(&self) -> u64 {
        (self.load() & WAITING) >> WAITING_SHIFT
    }
    pub fn claim(&self) -> Optimistic<ExclusiveGuard<'_, T, W>> {
        for _ in 0..SPIN_BUDGET {
            let word = self.load();
            if word & STATE == 0
                && self
                    .version
                    .compare_exchange_weak(
                        word,
                        word | EXCLUSIVE,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return Ok(ExclusiveGuard {
                    latch: self,
                    taken_unwinding: std::thread::panicking(),
                });
            }
            spin();
        }
        Err(Unwind)
    }
    pub fn try_exclusive(&self) -> Optimistic<ExclusiveGuard<'_, T, W>> {
        let version = self.load();
        if version & (STATE | WAITING) != 0 {
            return Err(Unwind);
        }
        if self
            .version
            .compare_exchange(version, version | EXCLUSIVE, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(Unwind);
        }
        Ok(ExclusiveGuard { latch: self, taken_unwinding: std::thread::panicking() })
    }
    pub fn exclusive(&self) -> Optimistic<ExclusiveGuard<'_, T, W>> {
        for _ in 0..SPIN_BUDGET {
            let version = self.load();
            if version & (STATE | WAITING) == 0
                && self
                    .version
                    .compare_exchange_weak(
                        version,
                        version | EXCLUSIVE,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return Ok(ExclusiveGuard {
                    latch: self,
                    taken_unwinding: std::thread::panicking(),
                });
            }
            spin();
        }
        Err(Unwind)
    }
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        let word = self.load();
        exclusive_in(word) && word & VERSION == VERSION
    }
    #[must_use]
    pub fn is_held(&self) -> bool {
        exclusive_in(self.load())
    }
    #[must_use]
    pub fn sharers(&self) -> u64 {
        let word = self.load();
        if exclusive_in(word) { 0 } else { word >> STATE_SHIFT }
    }
    #[must_use]
    pub fn unchanged_since(&self, version: u64) -> bool {
        fence(Ordering::Acquire);
        let word = self.load();
        !exclusive_in(word) && word & VERSION == version
    }
    #[must_use]
    pub fn version(&self) -> u64 {
        self.load() & VERSION
    }
    fn load_words(&self, order: Ordering) -> [u64; W] {
        core::array::from_fn(|i| self.words[i].load(order))
    }
    fn pair_at(&self, at: usize) -> Option<(u64, u64)> {
        let first = at / 8;
        if first + 1 >= W {
            return None;
        }
        Some((
            self.words[first].load(Ordering::Acquire),
            self.words[first + 1].load(Ordering::Acquire),
        ))
    }
    fn word(&self, w: usize) -> Option<u64> {
        if w >= W {
            return None;
        }
        Some(self.words[w].load(Ordering::Acquire))
    }
    fn peek_fixed<const N: usize>(&self, at: usize) -> Option<[u8; N]> {
        let end = at.checked_add(N)?;
        if end > W * 8 {
            return None;
        }
        let mut out = [0_u8; N];
        let (first, last) = (at / 8, (end - 1) / 8);
        let mut byte = at % 8;
        let mut written = 0;
        for w in first..=last {
            let word = self.words[w].load(Ordering::Acquire).to_le_bytes();
            let take = (8 - byte).min(N - written);
            out[written..written + take].copy_from_slice(&word[byte..byte + take]);
            written += take;
            byte = 0;
        }
        Some(out)
    }
    fn peek_words(&self, at: usize, out: &mut [u8]) -> bool {
        match at.checked_add(out.len()) {
            Some(e) if e <= W * 8 => {}
            _ => return false,
        }
        if out.is_empty() {
            return true;
        }
        if out.len() <= SHORT {
            let (first, skip) = (at / 8, at % 8);
            let mut covering = [0_u8; SHORT + 8];
            for (i, word) in self.words[first..(at + out.len()).div_ceil(8)].iter().enumerate() {
                covering[8 * i..8 * i + 8]
                    .copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
            }
            copy_bytes(out, &covering[skip..skip + out.len()]);
            return true;
        }
        let mut w = at / 8;
        let mut written = 0;
        let head = at % 8;
        if head != 0 {
            let word = self.words[w].load(Ordering::Relaxed).to_le_bytes();
            let take = 8 - head;
            copy_bytes(&mut out[..take], &word[head..]);
            written = take;
            w += 1;
        }
        let whole = (out.len() - written) / 8;
        for (chunk, word) in out[written..written + 8 * whole]
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(&self.words[w..w + whole])
        {
            *chunk = word.load(Ordering::Relaxed).to_le_bytes();
        }
        w += whole;
        written += 8 * whole;
        if written < out.len() {
            let word = self.words[w].load(Ordering::Relaxed).to_le_bytes();
            let take = out.len() - written;
            copy_bytes(&mut out[written..], &word[..take]);
        }
        true
    }
    fn store_words(&self, src: &[u64; W]) {
        for (i, w) in src.iter().enumerate() {
            self.words[i].store(*w, Ordering::Release);
        }
    }
    fn store_word_range(&self, src: &[u64; W], start: usize, end: usize) {
        let first = start / 8;
        let last = ((end - 1) / 8).min(W - 1);
        for (word, value) in self.words[first..=last].iter().zip(&src[first..=last]) {
            word.store(*value, Ordering::Release);
        }
    }
    fn poke_words(&self, at: usize, src: &[u8]) -> bool {
        match at.checked_add(src.len()) {
            Some(e) if e <= W * 8 => {}
            _ => return false,
        }
        if src.is_empty() {
            return true;
        }
        let end = at + src.len();
        let (first, last) = (at / 8, (end - 1) / 8);
        let mut byte = at % 8;
        let mut taken = 0;
        for w in first..=last {
            let take = (8 - byte).min(src.len() - taken);
            let word = if take == 8 {
                u64::from_le_bytes(src[taken..taken + 8].try_into().unwrap_or([0; 8]))
            } else {
                let mut bytes = self.words[w].load(HELD).to_le_bytes();
                bytes[byte..byte + take].copy_from_slice(&src[taken..taken + take]);
                u64::from_le_bytes(bytes)
            };
            self.words[w].store(word, Ordering::Release);
            taken += take;
            byte = 0;
        }
        true
    }
}
const SHORT: usize = 16;
#[inline]
pub(crate) fn copy_bytes(out: &mut [u8], from: &[u8]) {
    debug_assert_eq!(out.len(), from.len(), "copy_bytes copies like for like");
    let n = out.len().min(from.len());
    match n {
        0 => {}
        1..=3 => {
            out[0] = from[0];
            out[n / 2] = from[n / 2];
            out[n - 1] = from[n - 1];
        }
        4..=7 => fixed::<4>(out, from),
        8..=SHORT => fixed::<8>(out, from),
        _ => out[..n].copy_from_slice(&from[..n]),
    }
}
#[inline]
fn fixed<const N: usize>(out: &mut [u8], from: &[u8]) {
    if let (Some(o), Some(f)) = (out.first_chunk_mut::<N>(), from.first_chunk::<N>()) {
        *o = *f;
    }
    if let (Some(o), Some(f)) = (out.last_chunk_mut::<N>(), from.last_chunk::<N>()) {
        *o = *f;
    }
}
fn to_words<T: AnyBits, const W: usize>(value: &T) -> [u64; W] {
    let mut buf = [0_u64; W];
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::from_ref(value).cast::<u8>(),
            buf.as_mut_ptr().cast::<u8>(),
            size_of::<T>(),
        );
    }
    buf
}
fn from_words<T: AnyBits, const W: usize>(buf: &[u64; W]) -> T {
    let mut out = MaybeUninit::<T>::uninit();
    unsafe {
        core::ptr::copy_nonoverlapping(
            buf.as_ptr().cast::<u8>(),
            out.as_mut_ptr().cast::<u8>(),
            size_of::<T>(),
        );
        out.assume_init()
    }
}
pub struct OptimisticGuard<'a, T: AnyBits, const W: usize> {
    latch: &'a HybridLatch<T, W>,
    version: u64,
}
impl<T: AnyBits, const W: usize> Clone for OptimisticGuard<'_, T, W> {
    fn clone(&self) -> Self {
        Self { latch: self.latch, version: self.version }
    }
}
impl<'a, T: AnyBits, const W: usize> OptimisticGuard<'a, T, W> {
    pub fn recheck(&self) -> Optimistic<()> {
        if self.latch.unchanged_since(self.version) { Ok(()) } else { Err(Unwind) }
    }
    #[must_use]
    pub fn sampled(&self) -> u64 {
        self.version
    }
    pub fn read(&self) -> Optimistic<T> {
        let words = self.latch.load_words(Ordering::Relaxed);
        self.recheck()?;
        Ok(from_words::<T, W>(&words))
    }
    pub fn peek(&self, at: usize, out: &mut [u8]) -> Optimistic<()> {
        if self.latch.peek_words(at, out) { Ok(()) } else { Err(Unwind) }
    }
    pub fn word_at(&self, w: usize) -> Optimistic<u64> {
        self.latch.word(w).ok_or(Unwind)
    }
    pub fn read_pair(&self, at: usize) -> Optimistic<(u64, u64)> {
        self.latch.pair_at(at).ok_or(Unwind)
    }
    pub fn read_bytes_unvalidated<const N: usize>(&self, at: usize) -> Optimistic<[u8; N]> {
        self.latch.peek_fixed::<N>(at).ok_or(Unwind)
    }
    pub fn read_bytes<const N: usize>(&self, at: usize) -> Optimistic<[u8; N]> {
        let mut out = [0_u8; N];
        self.peek(at, &mut out)?;
        self.recheck()?;
        Ok(out)
    }
    pub fn upgrade(self) -> Optimistic<ExclusiveGuard<'a, T, W>> {
        if self
            .latch
            .version
            .compare_exchange(
                self.version,
                self.version | EXCLUSIVE,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Err(Unwind);
        }
        Ok(ExclusiveGuard { latch: self.latch, taken_unwinding: std::thread::panicking() })
    }
    pub fn try_shared(self) -> Optimistic<SharedGuard<'a, T, W>> {
        let mut word = self.latch.version.load(Ordering::Relaxed);
        for _ in 0..SPIN_BUDGET {
            if exclusive_in(word)
                || word & VERSION != self.version
                || word >> STATE_SHIFT >= SHARED_MAX
            {
                return Err(Unwind);
            }
            match self.latch.version.compare_exchange_weak(
                word,
                word + ONE_SHARER,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(SharedGuard { latch: self.latch });
                }
                Err(now) => {
                    word = now;
                    spin();
                }
            }
        }
        Err(Unwind)
    }
}
pub struct Waiting<'a> {
    word: &'a AtomicU64,
}
impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.word.fetch_sub(ONE_WAITER, Ordering::Relaxed);
    }
}
pub struct SharedGuard<'a, T: AnyBits, const W: usize> {
    latch: &'a HybridLatch<T, W>,
}
impl<T: AnyBits, const W: usize> SharedGuard<'_, T, W> {
    #[must_use]
    pub fn get(&self) -> T {
        from_words::<T, W>(&self.latch.load_words(HELD))
    }
    pub fn peek(&self, at: usize, out: &mut [u8]) -> Optimistic<()> {
        if self.latch.peek_words(at, out) { Ok(()) } else { Err(Unwind) }
    }
    pub fn read_pair(&self, at: usize) -> Optimistic<(u64, u64)> {
        self.latch.pair_at(at).ok_or(Unwind)
    }
    pub fn read_bytes_unvalidated<const N: usize>(&self, at: usize) -> Optimistic<[u8; N]> {
        self.latch.peek_fixed::<N>(at).ok_or(Unwind)
    }
}
impl<T: AnyBits, const W: usize> Drop for SharedGuard<'_, T, W> {
    fn drop(&mut self) {
        self.latch.version.fetch_sub(ONE_SHARER, Ordering::Release);
    }
}
pub struct ExclusiveGuard<'a, T: AnyBits, const W: usize> {
    latch: &'a HybridLatch<T, W>,
    taken_unwinding: bool,
}
impl<T: AnyBits, const W: usize> ExclusiveGuard<'_, T, W> {
    #[must_use]
    pub fn get(&self) -> T {
        from_words::<T, W>(&self.latch.load_words(HELD))
    }
    pub fn peek(&self, at: usize, out: &mut [u8]) -> Optimistic<()> {
        if self.latch.peek_words(at, out) { Ok(()) } else { Err(Unwind) }
    }
    pub fn read_pair(&self, at: usize) -> Optimistic<(u64, u64)> {
        self.latch.pair_at(at).ok_or(Unwind)
    }
    pub fn read_bytes_unvalidated<const N: usize>(&self, at: usize) -> Optimistic<[u8; N]> {
        self.latch.peek_fixed::<N>(at).ok_or(Unwind)
    }
    #[must_use]
    pub fn bytes_into(&self, into: &mut [u8]) -> bool {
        if into.len() != W * 8 {
            return false;
        }
        for (word, slot) in self.latch.load_words(HELD).iter().zip(into.as_chunks_mut::<8>().0) {
            slot.copy_from_slice(&word.to_ne_bytes());
        }
        true
    }
    pub fn set(&mut self, value: T) {
        self.latch.store_words(&to_words::<T, W>(&value));
    }
    pub fn overwrite(&mut self, value: &T) {
        self.latch.store_words(&to_words::<T, W>(value));
    }
    pub fn update_ranges<R, const N: usize>(
        &mut self,
        f: impl FnOnce(&mut T) -> (R, [(usize, usize); N]),
    ) -> R {
        let mut words = self.latch.load_words(HELD);
        const { assert!(align_of::<T>() <= align_of::<u64>()) };
        let value: &mut T = unsafe { &mut *words.as_mut_ptr().cast::<T>() };
        let (out, ranges) = f(value);
        for (start, end) in ranges {
            if start >= end {
                continue;
            }
            self.latch.store_word_range(&words, start, end);
        }
        out
    }
    pub fn write_at(&mut self, at: usize, src: &[u8]) -> bool {
        self.latch.poke_words(at, src)
    }
    #[must_use]
    pub fn word_at(&self, w: usize) -> Option<u64> {
        self.latch.word(w)
    }
    pub fn update<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut words = self.latch.load_words(HELD);
        {
            const { assert!(align_of::<T>() <= align_of::<u64>()) };
            let value: &mut T = unsafe { &mut *words.as_mut_ptr().cast::<T>() };
            let out = f(value);
            self.latch.store_words(&words);
            out
        }
    }
}
impl<T: AnyBits, const W: usize> Drop for ExclusiveGuard<'_, T, W> {
    fn drop(&mut self) {
        if std::thread::panicking() && !self.taken_unwinding {
            let mut word = self.latch.version.load(Ordering::Relaxed);
            while let Err(now) = self.latch.version.compare_exchange_weak(
                word,
                word | VERSION,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                word = now;
            }
            return;
        }
        let mut word = self.latch.version.load(Ordering::Relaxed);
        loop {
            match self.latch.version.compare_exchange_weak(
                word,
                released(word),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => word = now,
            }
        }
    }
}
