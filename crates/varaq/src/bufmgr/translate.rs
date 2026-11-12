use crate::frame::NO_FRAME;
use crate::latch::HybridLatch;
use crate::{Optimistic, PageId, Unwind};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
pub type BucketLatch = HybridLatch<[u8; 0], 0>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Yes,
    Gone,
    Elsewhere(u32),
}
const VACANT: u64 = 0;
fn hold(bucket: &Bucket) -> crate::latch::ExclusiveGuard<'_, [u8; 0], 0> {
    loop {
        if let Ok(guard) = bucket.latch.exclusive() {
            fence(Ordering::Release);
            return guard;
        }
        crate::latch::spin();
    }
}
#[derive(Debug, Default)]
struct Link {
    page: AtomicU64,
    next: AtomicU32,
}
struct Bucket {
    latch: BucketLatch,
    page: AtomicU64,
    frame: AtomicU32,
    next: AtomicU32,
}
impl Default for Bucket {
    fn default() -> Self {
        Self {
            latch: BucketLatch::new([]),
            page: AtomicU64::new(VACANT),
            frame: AtomicU32::new(NO_FRAME),
            next: AtomicU32::new(NO_FRAME),
        }
    }
}
#[allow(clippy::missing_fields_in_debug)]
impl core::fmt::Debug for Bucket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bucket")
            .field("page", &self.page.load(Ordering::Relaxed))
            .field("frame", &self.frame.load(Ordering::Relaxed))
            .field("next", &self.next.load(Ordering::Relaxed))
            .finish()
    }
}
#[derive(Debug)]
pub struct Translation {
    buckets: Box<[Bucket]>,
    links: Box<[Link]>,
    mask: u64,
}
#[inline]
#[must_use]
pub const fn mix(page: u64) -> u64 {
    let mut h = page;
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}
#[inline]
#[must_use]
pub const fn preferred_frame_of(hash: u64, frames: u32) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    {
        ((hash as u128 * frames as u128) >> 64) as u32
    }
}
#[inline]
#[must_use]
pub const fn preferred_frame(page: PageId, frames: u32) -> u32 {
    preferred_frame_of(mix(page.get()), frames)
}
impl Translation {
    #[must_use]
    pub fn new(frames: usize) -> Self {
        let buckets = (frames.saturating_mul(2).max(1)).next_power_of_two();
        Self {
            buckets: (0..buckets).map(|_| Bucket::default()).collect(),
            links: (0..frames).map(|_| Link::default()).collect(),
            mask: (buckets - 1) as u64,
        }
    }
    #[must_use]
    pub const fn buckets(&self) -> usize {
        self.buckets.len()
    }
    #[inline]
    fn bucket_of(&self, page: PageId) -> &Bucket {
        self.bucket_of_hash(mix(page.get()))
    }
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn bucket_of_hash(&self, hash: u64) -> &Bucket {
        &self.buckets[(hash & self.mask) as usize]
    }
    pub fn lookup(&self, page: PageId) -> Optimistic<Option<u32>> {
        self.lookup_hashed(page, mix(page.get()))
    }
    pub fn lookup_hashed(&self, page: PageId, hash: u64) -> Optimistic<Option<u32>> {
        let bucket = self.bucket_of_hash(hash);
        let guard = bucket.latch.optimistic()?;
        let target = page.get();
        let mut found = None;
        if bucket.page.load(Ordering::Relaxed) == target {
            found = Some(bucket.frame.load(Ordering::Relaxed));
        } else {
            let mut at = bucket.next.load(Ordering::Relaxed);
            let mut steps = self.links.len();
            while at != NO_FRAME && steps > 0 {
                let Some(link) = self.links.get(at as usize) else {
                    return Err(Unwind);
                };
                if link.page.load(Ordering::Relaxed) == target {
                    found = Some(at);
                    break;
                }
                at = link.next.load(Ordering::Relaxed);
                steps -= 1;
            }
            if steps == 0 {
                return Err(Unwind);
            }
        }
        guard.recheck()?;
        Ok(found)
    }
    pub fn insert(&self, page: PageId, frame: u32) -> Optimistic<()> {
        self.insert_if(page, frame, || true, |_| false).map(|_| ())
    }
    pub fn insert_if(
        &self,
        page: PageId,
        frame: u32,
        still: impl Fn() -> bool,
        holds: impl Fn(u32) -> bool,
    ) -> Optimistic<Recorded> {
        if frame as usize >= self.links.len() {
            return Err(Unwind);
        }
        let bucket = self.bucket_of(page);
        let guard = hold(bucket);
        if !still() {
            drop(guard);
            return Ok(Recorded::Gone);
        }
        let target = page.get();
        if bucket.page.load(Ordering::Relaxed) == target {
            let named = bucket.frame.load(Ordering::Relaxed);
            if named != frame && holds(named) {
                drop(guard);
                return Ok(Recorded::Elsewhere(named));
            }
            bucket.frame.store(frame, Ordering::Release);
            drop(guard);
            return Ok(Recorded::Yes);
        }
        let mut prev = None;
        let mut at = bucket.next.load(Ordering::Relaxed);
        while at != NO_FRAME {
            let link = self.links.get(at as usize).ok_or(Unwind)?;
            let next = link.next.load(Ordering::Relaxed);
            if link.page.load(Ordering::Relaxed) == target {
                if at == frame {
                    drop(guard);
                    return Ok(Recorded::Yes);
                }
                if holds(at) {
                    drop(guard);
                    return Ok(Recorded::Elsewhere(at));
                }
                let moved = self.links.get(frame as usize).ok_or(Unwind)?;
                moved.page.store(target, Ordering::Relaxed);
                moved.next.store(next, Ordering::Relaxed);
                match prev {
                    None => bucket.next.store(frame, Ordering::Release),
                    Some(p) => {
                        let prev_link: &Link = self.links.get(p as usize).ok_or(Unwind)?;
                        prev_link.next.store(frame, Ordering::Release);
                    }
                }
                link.page.store(VACANT, Ordering::Relaxed);
                drop(guard);
                return Ok(Recorded::Yes);
            }
            prev = Some(at);
            at = next;
        }
        if bucket.page.load(Ordering::Relaxed) == VACANT {
            bucket.frame.store(frame, Ordering::Relaxed);
            bucket.page.store(target, Ordering::Release);
        } else {
            let link = self.links.get(frame as usize).ok_or(Unwind)?;
            link.page.store(target, Ordering::Relaxed);
            link.next.store(bucket.next.load(Ordering::Relaxed), Ordering::Relaxed);
            bucket.next.store(frame, Ordering::Release);
        }
        drop(guard);
        Ok(Recorded::Yes)
    }
    pub(crate) fn bucket_latch(&self, hash: u64) -> &BucketLatch {
        &self.bucket_of_hash(hash).latch
    }
    pub fn remove(&self, page: PageId) -> Optimistic<bool> {
        self.remove_where(page, None)
    }
    pub fn remove_named(&self, page: PageId, frame: u32) -> Optimistic<bool> {
        self.remove_where(page, Some(frame))
    }
    fn remove_where(&self, page: PageId, only: Option<u32>) -> Optimistic<bool> {
        let bucket = self.bucket_of(page);
        let guard = hold(bucket);
        let target = page.get();
        if bucket.page.load(Ordering::Relaxed) == target {
            if only.is_some_and(|f| bucket.frame.load(Ordering::Relaxed) != f) {
                drop(guard);
                return Ok(false);
            }
            let head = bucket.next.load(Ordering::Relaxed);
            if head == NO_FRAME {
                bucket.page.store(VACANT, Ordering::Release);
            } else {
                let link = self.links.get(head as usize).ok_or(Unwind)?;
                bucket.frame.store(head, Ordering::Relaxed);
                bucket.next.store(link.next.load(Ordering::Relaxed), Ordering::Relaxed);
                bucket.page.store(link.page.load(Ordering::Relaxed), Ordering::Release);
                link.page.store(VACANT, Ordering::Relaxed);
            }
            drop(guard);
            return Ok(true);
        }
        let mut prev = None;
        let mut at = bucket.next.load(Ordering::Relaxed);
        while at != NO_FRAME {
            let link = self.links.get(at as usize).ok_or(Unwind)?;
            let next = link.next.load(Ordering::Relaxed);
            if link.page.load(Ordering::Relaxed) == target {
                if only.is_some_and(|f| at != f) {
                    drop(guard);
                    return Ok(false);
                }
                match prev {
                    None => bucket.next.store(next, Ordering::Release),
                    Some(p) => {
                        let prev_link: &Link = self.links.get(p as usize).ok_or(Unwind)?;
                        prev_link.next.store(next, Ordering::Release);
                    }
                }
                link.page.store(VACANT, Ordering::Relaxed);
                drop(guard);
                return Ok(true);
            }
            prev = Some(at);
            at = next;
        }
        drop(guard);
        Ok(false)
    }
    #[must_use]
    pub fn len(&self) -> usize {
        let mut n = 0;
        for bucket in &self.buckets {
            if bucket.page.load(Ordering::Relaxed) != VACANT {
                n += 1;
            }
            let mut at = bucket.next.load(Ordering::Relaxed);
            let mut steps = self.links.len();
            while at != NO_FRAME && steps > 0 {
                let Some(link) = self.links.get(at as usize) else { break };
                if link.page.load(Ordering::Relaxed) != VACANT {
                    n += 1;
                }
                at = link.next.load(Ordering::Relaxed);
                steps -= 1;
            }
        }
        n
    }
    #[must_use]
    pub fn bucket_addresses(&self) -> core::ops::Range<usize> {
        let range = self.buckets.as_ptr_range();
        range.start as usize..range.end as usize
    }
    #[must_use]
    pub fn entries(&self) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        for bucket in &self.buckets {
            let page = bucket.page.load(Ordering::Relaxed);
            if page != VACANT {
                out.push((page, bucket.frame.load(Ordering::Relaxed)));
            }
            let mut at = bucket.next.load(Ordering::Relaxed);
            let mut steps = self.links.len();
            while at != NO_FRAME && steps > 0 {
                let Some(link) = self.links.get(at as usize) else { break };
                let p = link.page.load(Ordering::Relaxed);
                if p != VACANT {
                    out.push((p, at));
                }
                at = link.next.load(Ordering::Relaxed);
                steps -= 1;
            }
        }
        out
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
