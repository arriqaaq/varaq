use crate::latch::{ExclusiveGuard, OptimisticGuard, SharedGuard};
use crate::node::{HEADER_BYTES, Node, PageKind, SLOT_BYTES, offsets};
use crate::{Optimistic, Unwind};
use core::cmp::Ordering;
const CHUNK: usize = 64;
pub struct NodeRef<'g, 'a, const W: usize> {
    guard: Through<'g, 'a, W>,
}
enum Through<'g, 'a, const W: usize> {
    Optimistic(&'g OptimisticGuard<'a, Node, W>),
    Held(&'g ExclusiveGuard<'a, Node, W>),
    Shared(&'g SharedGuard<'a, Node, W>),
}
impl<const W: usize> Through<'_, '_, W> {
    fn peek(&self, at: usize, out: &mut [u8]) -> Optimistic<()> {
        match self {
            Self::Optimistic(g) => g.peek(at, out),
            Self::Held(g) => g.peek(at, out),
            Self::Shared(g) => g.peek(at, out),
        }
    }
    fn read_pair(&self, at: usize) -> Optimistic<(u64, u64)> {
        match self {
            Self::Optimistic(g) => g.read_pair(at),
            Self::Held(g) => g.read_pair(at),
            Self::Shared(g) => g.read_pair(at),
        }
    }
    fn read_bytes_unvalidated<const N: usize>(&self, at: usize) -> Optimistic<[u8; N]> {
        match self {
            Self::Optimistic(g) => g.read_bytes_unvalidated::<N>(at),
            Self::Held(g) => g.read_bytes_unvalidated::<N>(at),
            Self::Shared(g) => g.read_bytes_unvalidated::<N>(at),
        }
    }
}
impl<'g, 'a, const W: usize> NodeRef<'g, 'a, W> {
    #[must_use]
    pub fn new(guard: &'g OptimisticGuard<'a, Node, W>) -> Self {
        Self { guard: Through::Optimistic(guard) }
    }
    #[must_use]
    pub fn held(guard: &'g ExclusiveGuard<'a, Node, W>) -> Self {
        Self { guard: Through::Held(guard) }
    }
    #[must_use]
    pub fn shared(guard: &'g SharedGuard<'a, Node, W>) -> Self {
        Self { guard: Through::Shared(guard) }
    }
    fn u16_at(&self, at: usize) -> Optimistic<u16> {
        Ok(u16::from_le_bytes(self.guard.read_bytes_unvalidated::<2>(at)?))
    }
    pub fn len(&self) -> Optimistic<usize> {
        Ok(self.u16_at(offsets::COUNT)? as usize)
    }
    pub fn is_empty(&self) -> Optimistic<bool> {
        Ok(self.len()? == 0)
    }
    pub fn kind(&self) -> Optimistic<u8> {
        Ok(self.guard.read_bytes_unvalidated::<1>(offsets::KIND)?[0])
    }
    pub fn blob_header(&self) -> Optimistic<(u64, u64, usize)> {
        let held = self.u16_at(offsets::SPACE_USED)? as usize;
        Ok((self.upper()?, self.right()?, held))
    }
    pub fn blob_into(&self, out: &mut [u8]) -> Optimistic<()> {
        self.guard.peek(offsets::DATA, out)
    }
    pub fn is_leaf(&self) -> Optimistic<bool> {
        Ok(self.kind()? == PageKind::Leaf as u8)
    }
    pub fn is_inner(&self) -> Optimistic<bool> {
        Ok(self.kind()? == PageKind::Inner as u8)
    }
    pub fn prefix_len(&self) -> Optimistic<usize> {
        Ok(self.u16_at(offsets::PREFIX_LEN)? as usize)
    }
    fn fence(&self, at: usize) -> Optimistic<(usize, usize)> {
        let offset = self.u16_at(at)? as usize;
        let len = self.u16_at(at + 2)? as usize;
        if offset < HEADER_BYTES || offset + len > W * 8 {
            return Err(Unwind);
        }
        Ok((offset, len))
    }
    fn cmp_bytes(&self, at: usize, len: usize, needle: &[u8]) -> Optimistic<Ordering> {
        let mut read = 0;
        let mut buf = [0_u8; CHUNK];
        while read < len && read < needle.len() {
            let take = CHUNK.min(len - read).min(needle.len() - read);
            self.guard.peek(at + read, &mut buf[..take])?;
            match buf[..take].cmp(&needle[read..read + take]) {
                Ordering::Equal => read += take,
                other => return Ok(other),
            }
        }
        Ok(len.cmp(&needle.len()))
    }
    fn key_wide(&self, at: usize, len: usize) -> Optimistic<u128> {
        let take = len.min(WIDE);
        let (base, skip) = (at - at % 8, at % 8);
        if base + 24 > W * 8 {
            let mut buf = [0_u8; WIDE];
            self.guard.peek(at, &mut buf[..take])?;
            return Ok(u128::from_be_bytes(buf));
        }
        let (w0, w1) = self.guard.read_pair(base)?;
        let w2 = if skip + take > 16 { self.guard.read_pair(base + 8)?.1 } else { 0 };
        Ok(wide_from_words([w0, w1, w2], skip, take))
    }
    pub fn upper(&self) -> Optimistic<u64> {
        Ok(u64::from_le_bytes(self.guard.read_bytes_unvalidated::<8>(offsets::UPPER)?))
    }
    pub fn right(&self) -> Optimistic<u64> {
        Ok(u64::from_le_bytes(self.guard.read_bytes_unvalidated::<8>(offsets::RIGHT)?))
    }
    pub fn covers(&self, key: &[u8]) -> Optimistic<bool> {
        let (lo_at, lo_len) = self.fence(offsets::LOWER_FENCE)?;
        let lower_below = lo_len != 0 && self.cmp_bytes(lo_at, lo_len, key)? == Ordering::Less;
        let (hi_at, hi_len) = self.fence(offsets::UPPER_FENCE)?;
        let at_or_below = hi_len != 0 && self.cmp_bytes(hi_at, hi_len, key)? != Ordering::Less;
        Ok(calc::covers_by(lo_len == 0, lower_below, hi_len == 0, at_or_below))
    }
    pub fn covers_from_start(&self, key: &[u8]) -> Optimistic<(bool, bool)> {
        let (lo_at, lo_len) = self.fence(offsets::LOWER_FENCE)?;
        let mut from_start = false;
        let lower_below = if lo_len == 0 {
            false
        } else if key.len() == lo_len + 1 && key[lo_len] == 0 {
            match self.cmp_bytes(lo_at, lo_len, &key[..lo_len])? {
                Ordering::Equal => {
                    from_start = true;
                    true
                }
                Ordering::Less => true,
                Ordering::Greater => false,
            }
        } else {
            self.cmp_bytes(lo_at, lo_len, key)? == Ordering::Less
        };
        let (hi_at, hi_len) = self.fence(offsets::UPPER_FENCE)?;
        let at_or_below = hi_len != 0 && self.cmp_bytes(hi_at, hi_len, key)? != Ordering::Less;
        if calc::covers_by(lo_len == 0, lower_below, hi_len == 0, at_or_below) {
            Ok((true, from_start))
        } else {
            Ok((false, false))
        }
    }
    pub fn slot(&self, i: usize) -> Optimistic<(usize, usize, usize, u32, u16)> {
        let at = offsets::DATA + i * SLOT_BYTES;
        let (w0, w1) = self.guard.read_pair(at - at % 8)?;
        checked_slot::<W>(u128::from(w0) | (u128::from(w1) << 64), at % 8)
    }
    pub fn slots_into(
        &self,
        from: usize,
        to: usize,
        out: &mut crate::linebuf::LineBuf<u8>,
    ) -> Optimistic<()> {
        let Some(count) = to.checked_sub(from) else { return Err(Unwind) };
        out.clear();
        out.extend_unfilled(count * SLOT_BYTES);
        self.guard.peek(offsets::DATA + from * SLOT_BYTES, out.as_mut_slice())
    }
    pub fn lower_bound(&self, key: &[u8]) -> Optimistic<(usize, bool)> {
        let n = self.len()?;
        if n * SLOT_BYTES + HEADER_BYTES > W * 8 {
            return Err(Unwind);
        }
        let p = self.prefix_len()?;
        if key.len() < p {
            let (lo_at, _) = self.fence(offsets::LOWER_FENCE)?;
            return Ok(match self.cmp_bytes(lo_at, key.len(), key)? {
                Ordering::Less => (n, false),
                Ordering::Equal | Ordering::Greater => (0, false),
            });
        }
        let suffix = &key[p..];
        let head = crate::node::head_of(suffix);
        let wide = wide_of(suffix);
        calc::lower_bound_by(n, |mid| {
            let (at, key_len, _, slot_head, _) = self.slot(mid)?;
            Ok(match slot_head.cmp(&head) {
                Ordering::Equal => self.cmp_key(at, key_len, suffix, wide)?,
                other => other,
            })
        })
    }
    fn cmp_key(&self, at: usize, len: usize, needle: &[u8], wide: u128) -> Optimistic<Ordering> {
        match cmp_wide(self.key_wide(at, len)?, len, wide, needle.len()) {
            Some(order) => Ok(order),
            None => self.cmp_bytes(at + WIDE, len - WIDE, &needle[WIDE..]),
        }
    }
    pub fn key_of(&self, i: usize) -> Optimistic<Vec<u8>> {
        let (at, key_len, _, _, _) = self.slot(i)?;
        let prefix_len = self.prefix_len()?;
        let (fence_at, fence_len) = self.fence(offsets::LOWER_FENCE)?;
        if prefix_len > fence_len {
            return Err(Unwind);
        }
        let mut out = vec![0_u8; prefix_len + key_len];
        if prefix_len > 0 {
            self.guard.peek(fence_at, &mut out[..prefix_len])?;
        }
        self.guard.peek(at, &mut out[prefix_len..])?;
        Ok(out)
    }
    pub fn payload_of(&self, i: usize) -> Optimistic<Vec<u8>> {
        let (at, key_len, payload_len, _, _) = self.slot(i)?;
        let mut out = vec![0_u8; payload_len];
        self.guard.peek(at + key_len, &mut out)?;
        Ok(out)
    }
    pub fn prefix_bytes(&self) -> Optimistic<Vec<u8>> {
        let prefix_len = self.prefix_len()?;
        let (fence_at, fence_len) = self.fence(offsets::LOWER_FENCE)?;
        if prefix_len > fence_len {
            return Err(Unwind);
        }
        let mut out = vec![0_u8; prefix_len];
        if prefix_len > 0 {
            self.guard.peek(fence_at, &mut out)?;
        }
        Ok(out)
    }
    pub fn entry_into(
        &self,
        (slots, k): (&[u8], usize),
        prefix: &[u8],
        out: &mut crate::linebuf::LineBuf<u8>,
        payload_most: usize,
    ) -> Optimistic<(u32, u32, u16)> {
        let Some(slot) = slots.get(k * SLOT_BYTES..(k + 1) * SLOT_BYTES) else {
            return Err(Unwind);
        };
        let mut pair = [0_u8; 16];
        pair[..SLOT_BYTES].copy_from_slice(slot);
        let (at, key_len, payload_len, _, flags) = checked_slot::<W>(u128::from_le_bytes(pair), 0)?;
        let start = out.len();
        let key_end = start + prefix.len() + key_len;
        let payload_end = key_end + payload_len.min(payload_most);
        out.extend_unfilled(payload_end);
        let out = out.as_mut_slice();
        crate::latch::copy_bytes(&mut out[start..start + prefix.len()], prefix);
        self.guard.peek(at, &mut out[start + prefix.len()..payload_end])?;
        let (Ok(key_end), Ok(payload_end)) = (u32::try_from(key_end), u32::try_from(payload_end))
        else {
            return Err(Unwind);
        };
        Ok((key_end, payload_end, flags))
    }
    pub fn upper_fence_bytes(&self) -> Optimistic<Vec<u8>> {
        let (at, len) = self.fence(offsets::UPPER_FENCE)?;
        let mut out = vec![0_u8; len];
        if len > 0 {
            self.guard.peek(at, &mut out)?;
        }
        Ok(out)
    }
    pub fn prefix_into(&self, out: &mut crate::linebuf::LineBuf<u8>) -> Optimistic<()> {
        let prefix_len = self.prefix_len()?;
        let (fence_at, fence_len) = self.fence(offsets::LOWER_FENCE)?;
        if prefix_len > fence_len {
            return Err(Unwind);
        }
        out.clear();
        out.resize(prefix_len);
        if prefix_len > 0 {
            self.guard.peek(fence_at, out.as_mut_slice())?;
        }
        Ok(())
    }
    pub fn upper_fence_into(&self, out: &mut crate::linebuf::LineBuf<u8>) -> Optimistic<()> {
        let (at, len) = self.fence(offsets::UPPER_FENCE)?;
        out.clear();
        out.resize(len);
        if len > 0 {
            self.guard.peek(at, out.as_mut_slice())?;
        }
        Ok(())
    }
    pub fn payload_at(&self, i: usize, out: &mut [u8]) -> Optimistic<usize> {
        let (at, key_len, payload_len, _, _) = self.slot(i)?;
        let take = payload_len.min(out.len());
        self.guard.peek(at + key_len, &mut out[..take])?;
        Ok(payload_len)
    }
    pub fn payload_equals(&self, i: usize, want: &[u8]) -> Optimistic<bool> {
        let (at, key_len, payload_len, _, _) = self.slot(i)?;
        if payload_len != want.len() {
            return Ok(false);
        }
        self.bytes_equal(at + key_len, want)
    }
    pub fn payload_starts_with(&self, i: usize, want: &[u8]) -> Optimistic<bool> {
        let (at, key_len, payload_len, _, _) = self.slot(i)?;
        if payload_len < want.len() {
            return Ok(false);
        }
        self.bytes_equal(at + key_len, want)
    }
    fn bytes_equal(&self, from: usize, want: &[u8]) -> Optimistic<bool> {
        let mut chunk = [0_u8; CHUNK * 4];
        for (n, part) in want.chunks(chunk.len()).enumerate() {
            let here = &mut chunk[..part.len()];
            self.guard.peek(from + n * CHUNK * 4, here)?;
            if here != part {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
const WIDE: usize = 16;
#[inline]
#[must_use]
pub(crate) fn wide_of(key: &[u8]) -> u128 {
    let mut buf = [0_u8; WIDE];
    let n = key.len().min(WIDE);
    buf[..n].copy_from_slice(&key[..n]);
    u128::from_be_bytes(buf)
}
#[inline]
#[must_use]
pub(crate) fn wide_from_words(words: [u64; 3], skip: usize, take: usize) -> u128 {
    debug_assert!(skip < 8 && take <= WIDE, "inside three words");
    let mut low = (u128::from(words[0]) | (u128::from(words[1]) << 64)) >> (skip * 8);
    if skip > 0 {
        low |= u128::from(words[2]) << ((16 - skip) * 8);
    }
    let mask = if take >= WIDE { u128::MAX } else { (1_u128 << (take * 8)) - 1 };
    (low & mask).swap_bytes()
}
#[inline]
#[must_use]
pub(crate) fn cmp_wide(a: u128, a_len: usize, b: u128, b_len: usize) -> Option<Ordering> {
    match a.cmp(&b) {
        Ordering::Equal if a_len >= WIDE && b_len >= WIDE => None,
        Ordering::Equal => Some(a_len.cmp(&b_len)),
        other => Some(other),
    }
}
#[inline]
fn checked_slot<const W: usize>(
    pair: u128,
    byte: usize,
) -> Optimistic<(usize, usize, usize, u32, u16)> {
    let (offset, key_len, payload_len, head, flags) = slot_fields(pair, byte);
    if offset < HEADER_BYTES || offset + key_len + payload_len > W * 8 {
        return Err(Unwind);
    }
    Ok((offset, key_len, payload_len, head, flags))
}
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn slot_fields(pair: u128, byte: usize) -> (usize, usize, usize, u32, u16) {
    use crate::node::slot_layout::{FLAGS, HEAD, KEY_LEN, OFFSET, PAYLOAD_LEN};
    let field = |(b, width): (usize, usize)| -> u64 {
        let shifted = pair >> ((byte + b) * 8);
        (shifted as u64) & (u64::MAX >> (64 - width * 8))
    };
    (
        field(OFFSET) as usize,
        field(KEY_LEN) as usize,
        field(PAYLOAD_LEN) as usize,
        field(HEAD) as u32,
        field(FLAGS) as u16,
    )
}
