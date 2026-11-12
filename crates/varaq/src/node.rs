#![allow(unsafe_code)]
use crate::latch::AnyBits;
pub const PAGE_SIZE: usize = 4096;
pub(crate) const HEADER_BYTES: usize = 48;
pub const DATA_BYTES: usize = PAGE_SIZE - HEADER_BYTES;
pub const SEPARATE_ABOVE: usize = DATA_BYTES / 8;
#[must_use]
pub fn separates(line: usize, key_len: usize, value_len: usize) -> bool {
    let Some(entry) = key_len.checked_add(value_len).and_then(|n| n.checked_add(SLOT_BYTES)) else {
        return false;
    };
    entry > line && value_len >= line / 2 && entry <= DATA_BYTES
}
pub const SLOT_BYTES: usize = 12;
pub mod offsets {
    pub const CHECKSUM: usize = 0;
    pub const COUNT: usize = 4;
    pub const SPACE_USED: usize = 6;
    #[allow(dead_code)]
    pub const DATA_OFFSET: usize = 8;
    pub const PREFIX_LEN: usize = 10;
    pub const KIND: usize = 12;
    #[allow(dead_code)]
    pub const PADDING: usize = 13;
    pub const LOWER_FENCE: usize = 16;
    pub const UPPER_FENCE: usize = 20;
    pub const UPPER: usize = 24;
    pub const RIGHT: usize = 32;
    pub const GSN: usize = 40;
    pub const DATA: usize = super::HEADER_BYTES;
}
pub mod slot_layout {
    pub const OFFSET: (usize, usize) = (0, 2);
    pub const KEY_LEN: (usize, usize) = (2, 2);
    pub const PAYLOAD_LEN: (usize, usize) = (4, 2);
    pub const FLAGS: (usize, usize) = (6, 2);
    pub const HEAD: (usize, usize) = (8, 4);
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageKind {
    Inner = 0,
    Leaf = 1,
    Value = 2,
    Blob = 3,
}
pub mod flags {
    pub const SEPARATED: u16 = 1;
    pub const BLOB: u16 = 2;
    pub const DEFINED: u16 = SEPARATED | BLOB;
    pub const OUT_OF_LINE: u16 = SEPARATED | BLOB;
}
pub mod separated {
    pub const REFERENCE_BYTES: usize = 8;
    pub const LOCAL_MOST: usize = 11;
    pub const PAYLOAD_MOST: usize = REFERENCE_BYTES + LOCAL_MOST;
    #[must_use]
    pub const fn well_formed_len(len: usize) -> bool {
        len >= REFERENCE_BYTES && len <= PAYLOAD_MOST
    }
    #[must_use]
    pub const fn local_len(keep: usize, value_len: usize) -> usize {
        let most = if keep < LOCAL_MOST { keep } else { LOCAL_MOST };
        if most < value_len { most } else { value_len }
    }
    #[must_use]
    pub fn encode(page: u64, value: &[u8], keep: usize) -> ([u8; PAYLOAD_MOST], usize) {
        let mut out = [0_u8; PAYLOAD_MOST];
        let local = local_len(keep, value.len());
        out[..REFERENCE_BYTES].copy_from_slice(&page.to_le_bytes());
        out[REFERENCE_BYTES..REFERENCE_BYTES + local].copy_from_slice(&value[..local]);
        (out, REFERENCE_BYTES + local)
    }
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<(u64, &[u8])> {
        if !well_formed_len(payload.len()) {
            return None;
        }
        let (reference, local) = payload.split_at(REFERENCE_BYTES);
        let mut raw = [0_u8; REFERENCE_BYTES];
        raw.copy_from_slice(reference);
        Some((u64::from_le_bytes(raw), local))
    }
}
pub mod blob {
    use super::DATA_BYTES;
    pub const REFERENCE_BYTES: usize = 24;
    pub const LOCAL_MOST: usize = super::separated::LOCAL_MOST;
    pub const PAYLOAD_MOST: usize = REFERENCE_BYTES + LOCAL_MOST;
    pub const PAGE_BYTES: usize = DATA_BYTES;
    #[must_use]
    pub const fn well_formed_len(len: usize) -> bool {
        len >= REFERENCE_BYTES && len <= PAYLOAD_MOST
    }
    pub const TOKEN_TAG: u8 = 3;
    pub const TOKEN_MOST: usize = 1 + PAYLOAD_MOST;
    #[must_use]
    pub fn token(r: Ref, kept: &[u8]) -> ([u8; TOKEN_MOST], usize) {
        let mut out = [0_u8; TOKEN_MOST];
        out[0] = TOKEN_TAG;
        let (payload, n) = encode(r, kept, kept.len());
        out[1..=n].copy_from_slice(&payload[..n]);
        (out, n + 1)
    }
    #[must_use]
    pub fn of_token(stored: &[u8]) -> Option<(Ref, &[u8])> {
        match stored.split_first() {
            Some((&TOKEN_TAG, payload)) => decode(payload),
            _ => None,
        }
    }
    #[must_use]
    pub const fn needed(key_len: usize, value_len: usize) -> bool {
        match key_len.checked_add(value_len) {
            Some(n) => match n.checked_add(super::SLOT_BYTES) {
                Some(entry) => entry > DATA_BYTES,
                None => true,
            },
            None => true,
        }
    }
    pub fn write_page(out: &mut [u8; super::PAGE_SIZE], serial: u64, index: u64, bytes: &[u8]) {
        use super::{HEADER_BYTES, PAGE_SIZE, PageKind, offsets};
        assert!(bytes.len() <= DATA_BYTES && serial != 0, "a run page holds at most a data area");
        out[..HEADER_BYTES].fill(0);
        let put16 = |out: &mut [u8; PAGE_SIZE], at: usize, v: u16| {
            out[at..at + 2].copy_from_slice(&v.to_le_bytes());
        };
        put16(out, offsets::SPACE_USED, bytes.len() as u16);
        put16(out, offsets::DATA_OFFSET, HEADER_BYTES as u16);
        out[offsets::KIND] = PageKind::Blob as u8;
        put16(out, offsets::LOWER_FENCE, PAGE_SIZE as u16);
        put16(out, offsets::UPPER_FENCE, PAGE_SIZE as u16);
        out[offsets::UPPER..offsets::UPPER + 8].copy_from_slice(&serial.to_le_bytes());
        out[offsets::RIGHT..offsets::RIGHT + 8].copy_from_slice(&index.to_le_bytes());
        out[HEADER_BYTES..HEADER_BYTES + bytes.len()].copy_from_slice(bytes);
        out[HEADER_BYTES + bytes.len()..].fill(0);
    }
    #[must_use]
    pub const fn pages_for(rest: usize) -> u64 {
        let pages = rest.div_ceil(PAGE_BYTES);
        (if pages == 0 { 1 } else { pages }) as u64
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Ref {
        pub first: u64,
        pub len: u64,
        pub serial: u64,
    }
    impl Ref {
        #[must_use]
        pub fn rest(&self, local: usize) -> usize {
            usize::try_from(self.len).unwrap_or(usize::MAX).saturating_sub(local)
        }
        #[must_use]
        pub fn span_from(&self, local: usize, page: crate::PageId) -> usize {
            let pages = pages_for(self.rest(local));
            let index = page.get().wrapping_sub(self.first);
            usize::try_from(pages.saturating_sub(index)).unwrap_or(usize::MAX).max(1)
        }
    }
    #[must_use]
    pub fn encode(r: Ref, value: &[u8], local: usize) -> ([u8; PAYLOAD_MOST], usize) {
        let mut out = [0_u8; PAYLOAD_MOST];
        out[..8].copy_from_slice(&r.first.to_le_bytes());
        out[8..16].copy_from_slice(&r.len.to_le_bytes());
        out[16..24].copy_from_slice(&r.serial.to_le_bytes());
        out[REFERENCE_BYTES..REFERENCE_BYTES + local].copy_from_slice(&value[..local]);
        (out, REFERENCE_BYTES + local)
    }
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<(Ref, &[u8])> {
        if !well_formed_len(payload.len()) {
            return None;
        }
        let word = |at: usize| {
            let mut raw = [0_u8; 8];
            raw.copy_from_slice(&payload[at..at + 8]);
            u64::from_le_bytes(raw)
        };
        let r = Ref { first: word(0), len: word(8), serial: word(16) };
        let local = &payload[REFERENCE_BYTES..];
        if r.first == 0 || r.serial == 0 || (local.len() as u64) > r.len {
            return None;
        }
        Some((r, local))
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Fence {
    offset: u16,
    len: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub offset: u16,
    pub key_len: u16,
    pub payload_len: u16,
    pub flags: u16,
    pub head: u32,
}
impl Slot {
    fn encode(self) -> [u8; SLOT_BYTES] {
        use slot_layout::{FLAGS, HEAD, KEY_LEN, OFFSET, PAYLOAD_LEN};
        let mut b = [0_u8; SLOT_BYTES];
        b[OFFSET.0..OFFSET.0 + OFFSET.1].copy_from_slice(&self.offset.to_le_bytes());
        b[KEY_LEN.0..KEY_LEN.0 + KEY_LEN.1].copy_from_slice(&self.key_len.to_le_bytes());
        b[PAYLOAD_LEN.0..PAYLOAD_LEN.0 + PAYLOAD_LEN.1]
            .copy_from_slice(&self.payload_len.to_le_bytes());
        b[FLAGS.0..FLAGS.0 + FLAGS.1].copy_from_slice(&self.flags.to_le_bytes());
        b[HEAD.0..HEAD.0 + HEAD.1].copy_from_slice(&self.head.to_le_bytes());
        b
    }
    fn decode(b: &[u8]) -> Self {
        use slot_layout::{FLAGS, HEAD, KEY_LEN, OFFSET, PAYLOAD_LEN};
        let u16_at = |at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
        Self {
            offset: u16_at(OFFSET.0),
            key_len: u16_at(KEY_LEN.0),
            payload_len: u16_at(PAYLOAD_LEN.0),
            flags: u16_at(FLAGS.0),
            head: u32::from_le_bytes([b[HEAD.0], b[HEAD.0 + 1], b[HEAD.0 + 2], b[HEAD.0 + 3]]),
        }
    }
}
fn shared_len(lower: &[u8], upper: &[u8]) -> usize {
    lower.iter().zip(upper.iter()).take_while(|(a, b)| a == b).count()
}
#[must_use]
pub fn head_of(key: &[u8]) -> u32 {
    let mut h = [0_u8; 4];
    let n = key.len().min(4);
    h[..n].copy_from_slice(&key[..n]);
    u32::from_be_bytes(h)
}
fn head_of_parts(head: &[u8], tail: &[u8]) -> u32 {
    let mut h = [0_u8; 4];
    let from_head = head.len().min(4);
    h[..from_head].copy_from_slice(&head[..from_head]);
    if from_head < 4 {
        let from_tail = tail.len().min(4 - from_head);
        h[from_head..from_head + from_tail].copy_from_slice(&tail[..from_tail]);
    }
    u32::from_be_bytes(h)
}
fn cmp_parts(stored: &[u8], head: &[u8], tail: &[u8]) -> core::cmp::Ordering {
    let n = stored.len().min(head.len());
    match stored[..n].cmp(&head[..n]) {
        core::cmp::Ordering::Equal => {}
        other => return other,
    }
    if stored.len() <= head.len() {
        return stored.len().cmp(&(head.len() + tail.len()));
    }
    let rest = &stored[head.len()..];
    let m = rest.len().min(tail.len());
    match rest[..m].cmp(&tail[..m]) {
        core::cmp::Ordering::Equal => rest.len().cmp(&tail.len()),
        other => other,
    }
}
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Node {
    checksum: u32,
    count: u16,
    space_used: u16,
    data_offset: u16,
    prefix_len: u16,
    kind: u8,
    _pad: [u8; 3],
    lower_fence: Fence,
    upper_fence: Fence,
    upper: u64,
    right: u64,
    gsn: u64,
    data: [u8; DATA_BYTES],
}
unsafe impl AnyBits for Node {}
const _: () = assert!(size_of::<Node>() == PAGE_SIZE, "a node is exactly one page");
impl Node {
    #[must_use]
    pub fn from_page_bytes(bytes: &[u8; PAGE_SIZE]) -> Self {
        unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<Self>()) }
    }
    #[must_use]
    pub fn to_page_bytes(&self) -> [u8; PAGE_SIZE] {
        unsafe { core::ptr::read_unaligned(core::ptr::from_ref(self).cast::<[u8; PAGE_SIZE]>()) }
    }
    #[must_use]
    pub fn new(kind: PageKind, lower: &[u8], upper: &[u8]) -> Self {
        Self::empty(kind as u8, lower, upper)
    }
    pub(crate) fn empty(kind: u8, lower: &[u8], upper: &[u8]) -> Self {
        let mut node = Self {
            checksum: 0,
            count: 0,
            space_used: 0,
            data_offset: PAGE_SIZE as u16,
            prefix_len: 0,
            kind,
            _pad: [0; 3],
            lower_fence: Fence::default(),
            upper_fence: Fence::default(),
            upper: 0,
            right: 0,
            gsn: 0,
            data: [0; DATA_BYTES],
        };
        node.lower_fence = node.store(lower);
        node.upper_fence = node.store(upper);
        node.prefix_len = shared_len(lower, upper) as u16;
        node
    }
    #[must_use]
    pub fn blob_page(serial: u64, index: u64, bytes: &[u8]) -> Self {
        let mut page = [0_u8; PAGE_SIZE];
        blob::write_page(&mut page, serial, index, bytes);
        Self::from_page_bytes(&page)
    }
    #[must_use]
    pub fn blob_header(&self) -> (u64, u64, usize) {
        (self.upper, self.right, self.space_used as usize)
    }
    #[must_use]
    pub fn blob_bytes(&self) -> &[u8] {
        &self.data[..(self.space_used as usize).min(DATA_BYTES)]
    }
    #[must_use]
    pub fn kind(&self) -> u8 {
        self.kind
    }
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        self.kind == PageKind::Leaf as u8
    }
    #[must_use]
    pub fn is_inner(&self) -> bool {
        self.kind == PageKind::Inner as u8
    }
    #[must_use]
    pub fn dirty_ranges(&self, before: (usize, usize), compacted: bool) -> [(usize, usize); 2] {
        let (before_len, before_data) = before;
        let slots_end = HEADER_BYTES + self.len().max(before_len) * SLOT_BYTES;
        let now = self.data_offset as usize;
        let heap = if compacted {
            (now.min(before_data), PAGE_SIZE)
        } else {
            (now.min(before_data), before_data.max(now))
        };
        [(0, slots_end.min(PAGE_SIZE)), heap]
    }
    #[must_use]
    pub fn extent(&self) -> (usize, usize) {
        (self.len(), self.data_offset as usize)
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    #[must_use]
    pub fn prefix(&self) -> &[u8] {
        let f = self.lower_fence;
        &self.data[Self::rel(f.offset)..Self::rel(f.offset) + self.prefix_len as usize]
    }
    #[must_use]
    pub fn lower_fence(&self) -> &[u8] {
        self.fence_bytes(self.lower_fence)
    }
    #[must_use]
    pub fn upper_fence(&self) -> &[u8] {
        self.fence_bytes(self.upper_fence)
    }
    #[must_use]
    pub fn covers(&self, key: &[u8]) -> bool {
        crate::calc::covers_by(
            self.lower_fence().is_empty(),
            key > self.lower_fence(),
            self.upper_fence().is_empty(),
            key <= self.upper_fence(),
        )
    }
    #[must_use]
    pub fn upper(&self) -> u64 {
        self.upper
    }
    pub fn set_upper(&mut self, child: u64) {
        self.upper = child;
    }
    #[must_use]
    pub fn right(&self) -> u64 {
        self.right
    }
    pub fn set_right(&mut self, next: u64) {
        self.right = next;
    }
    #[must_use]
    pub fn gsn(&self) -> u64 {
        self.gsn
    }
    pub fn set_gsn(&mut self, gsn: u64) {
        self.gsn = gsn;
    }
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let count = self.count as usize;
        let data_offset = self.data_offset as usize;
        if HEADER_BYTES + count * SLOT_BYTES > data_offset || data_offset > PAGE_SIZE {
            return false;
        }
        if self.prefix_len > self.lower_fence.len {
            return false;
        }
        let within = |offset: u16, len: u16| {
            let o = offset as usize;
            o >= HEADER_BYTES && o + len as usize <= PAGE_SIZE
        };
        if !within(self.lower_fence.offset, self.lower_fence.len)
            || !within(self.upper_fence.offset, self.upper_fence.len)
        {
            return false;
        }
        if self.prefix_len as usize != shared_len(self.lower_fence(), self.upper_fence()) {
            return false;
        }
        let leaf = match self.kind {
            k if k == PageKind::Inner as u8 => false,
            k if k == PageKind::Leaf as u8 => true,
            k if k == PageKind::Value as u8 => {
                if self.lower_fence.len != 0
                    || self.upper_fence.len != 0
                    || self.prefix_len != 0
                    || self.upper != 0
                    || self.right != 0
                {
                    return false;
                }
                false
            }
            k if k == PageKind::Blob as u8 => {
                return count == 0
                    && self.lower_fence.len == 0
                    && self.upper_fence.len == 0
                    && self.prefix_len == 0
                    && self.upper != 0
                    && data_offset == HEADER_BYTES
                    && self.space_used as usize <= DATA_BYTES;
            }
            _ => return false,
        };
        (0..count).all(|i| {
            let slot = self.slot(i);
            if !within(slot.offset, slot.key_len.saturating_add(slot.payload_len)) {
                return false;
            }
            match slot.flags {
                0 => true,
                flags::SEPARATED
                    if leaf && separated::well_formed_len(slot.payload_len as usize) =>
                {
                    let at = Self::rel(slot.offset) + slot.key_len as usize;
                    self.data[at..at + 8] != [0; 8]
                }
                flags::BLOB if leaf && blob::well_formed_len(slot.payload_len as usize) => {
                    let at = Self::rel(slot.offset) + slot.key_len as usize;
                    blob::decode(&self.data[at..at + slot.payload_len as usize]).is_some()
                }
                _ => false,
            }
        })
    }
    #[must_use]
    pub fn free_space(&self) -> usize {
        let slots_end = HEADER_BYTES + self.len() * SLOT_BYTES;
        (self.data_offset as usize).saturating_sub(slots_end)
    }
    fn fence_bytes(&self, f: Fence) -> &[u8] {
        &self.data[Self::rel(f.offset)..Self::rel(f.offset) + f.len as usize]
    }
    pub(crate) fn rel(offset: u16) -> usize {
        offset as usize - HEADER_BYTES
    }
    #[must_use]
    pub fn prefix_len(&self) -> u16 {
        self.prefix_len
    }
    #[must_use]
    pub fn flags_at(&self, i: usize) -> u16 {
        self.slot(i).flags
    }
    #[must_use]
    pub fn head_at(&self, i: usize) -> u32 {
        self.slot(i).head
    }
    fn slot(&self, i: usize) -> Slot {
        let at = i * SLOT_BYTES;
        Slot::decode(&self.data[at..at + SLOT_BYTES])
    }
    fn set_slot(&mut self, i: usize, s: Slot) {
        let at = i * SLOT_BYTES;
        self.data[at..at + SLOT_BYTES].copy_from_slice(&s.encode());
    }
    #[must_use]
    pub fn key_at(&self, i: usize) -> &[u8] {
        let s = self.slot(i);
        &self.data[Self::rel(s.offset)..Self::rel(s.offset) + s.key_len as usize]
    }
    #[must_use]
    pub fn payload_at(&self, i: usize) -> &[u8] {
        let s = self.slot(i);
        let start = Self::rel(s.offset) + s.key_len as usize;
        &self.data[start..start + s.payload_len as usize]
    }
    #[must_use]
    pub fn replace_payload(&mut self, i: usize, payload: &[u8]) -> bool {
        if i >= self.len() {
            return false;
        }
        let s = self.slot(i);
        if s.payload_len as usize != payload.len() {
            return false;
        }
        let start = Self::rel(s.offset) + s.key_len as usize;
        self.data[start..start + payload.len()].copy_from_slice(payload);
        true
    }
    #[must_use]
    pub fn replaces_in_place(old: (u16, usize), flags: u16, len: usize) -> bool {
        old.0 == flags && old.1 == len
    }
    fn store(&mut self, bytes: &[u8]) -> Fence {
        let len = bytes.len() as u16;
        self.data_offset -= len;
        let at = Self::rel(self.data_offset);
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
        self.space_used += len;
        Fence { offset: self.data_offset, len }
    }
    #[must_use]
    pub fn lower_bound(&self, key: &[u8]) -> (usize, bool) {
        let p = self.prefix_len as usize;
        if key.len() < p {
            let prefix = self.prefix();
            return if key <= &prefix[..key.len()] { (0, false) } else { (self.len(), false) };
        }
        debug_assert!(
            key[..p] == *self.prefix(),
            "lower_bound on a node that does not cover the key"
        );
        let suffix = &key[p..];
        let head = head_of(suffix);
        let found = crate::calc::lower_bound_by(self.len(), |mid| {
            Ok::<_, core::convert::Infallible>(match self.slot(mid).head.cmp(&head) {
                core::cmp::Ordering::Equal => self.key_at(mid).cmp(suffix),
                other => other,
            })
        });
        match found {
            Ok(answer) => answer,
            Err(never) => match never {},
        }
    }
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        match self.lower_bound(key) {
            (i, true) => Some(self.payload_at(i)),
            _ => None,
        }
    }
    #[must_use]
    pub fn has_room_for(&self, key: &[u8], payload: &[u8]) -> bool {
        self.room_parts(key, b"", payload.len()).is_some()
    }
    pub fn insert(&mut self, key: &[u8], payload: &[u8]) -> bool {
        self.insert_reporting(key, payload, 0).0
    }
    pub fn insert_reporting(&mut self, key: &[u8], payload: &[u8], flags: u16) -> (bool, bool) {
        self.insert_parts_reporting(key, b"", payload, flags)
    }
}
impl core::fmt::Debug for Node {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Node")
            .field("kind", &self.kind)
            .field("count", &self.count)
            .field("prefix_len", &self.prefix_len)
            .field("free", &self.free_space())
            .finish_non_exhaustive()
    }
}
pub mod parts;
pub mod reader;
pub mod remove;
pub mod structure;
