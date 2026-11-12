use crate::Error;
use crate::PAGE_SIZE;
use crate::wal::record::Kind;
pub const IMAGE_PREFIX_BYTES: usize = 4;
pub const IMAGE_MAX_BYTES: usize = IMAGE_PREFIX_BYTES + PAGE_SIZE;
pub const NO_GAP: (usize, usize) = (PAGE_SIZE, PAGE_SIZE);
#[must_use]
pub const fn gap_named(count: u16, data_offset: u16) -> (usize, usize) {
    let slots_end = crate::node::HEADER_BYTES + count as usize * crate::node::SLOT_BYTES;
    let top = data_offset as usize;
    if slots_end < top && top <= PAGE_SIZE { (slots_end, top) } else { NO_GAP }
}
#[must_use]
pub fn gap_of_page(page: &[u8; PAGE_SIZE]) -> (usize, usize) {
    gap_of_header(page).unwrap_or(NO_GAP)
}
fn gap_of_header(page: &[u8]) -> Option<(usize, usize)> {
    use crate::node::offsets::{COUNT, DATA_OFFSET};
    let field = |at: usize| page.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    Some(gap_named(field(COUNT)?, field(DATA_OFFSET)?))
}
pub fn image_prefix(gap: (usize, usize)) -> Result<[u8; IMAGE_PREFIX_BYTES], Error> {
    let (start, end) = gap;
    if start > end || end > PAGE_SIZE {
        return Err(Error::Corrupt);
    }
    let start = u16::try_from(start).map_err(|_| Error::Corrupt)?;
    let end = u16::try_from(end).map_err(|_| Error::Corrupt)?;
    let mut prefix = [0u8; IMAGE_PREFIX_BYTES];
    prefix[..2].copy_from_slice(&start.to_le_bytes());
    prefix[2..].copy_from_slice(&end.to_le_bytes());
    Ok(prefix)
}
#[must_use]
pub const fn image_len(gap: (usize, usize)) -> usize {
    IMAGE_PREFIX_BYTES + gap.0 + (PAGE_SIZE - gap.1)
}
pub fn image_of_page(page: &[u8]) -> Result<Vec<u8>, Error> {
    if page.len() != PAGE_SIZE {
        return Err(Error::Corrupt);
    }
    let gap = gap_of_header(page).ok_or(Error::Corrupt)?;
    let mut out = Vec::with_capacity(image_len(gap));
    out.extend_from_slice(&image_prefix(gap)?);
    out.extend_from_slice(&page[..gap.0]);
    out.extend_from_slice(&page[gap.1..]);
    Ok(out)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image<'a> {
    pub head: &'a [u8],
    pub tail: &'a [u8],
}
impl Image<'_> {
    #[must_use]
    pub const fn gap(&self) -> (usize, usize) {
        (self.head.len(), PAGE_SIZE - self.tail.len())
    }
    pub fn restore(&self, into: &mut [u8]) -> Result<(), Error> {
        if into.len() != PAGE_SIZE {
            return Err(Error::Corrupt);
        }
        let (start, end) = self.gap();
        into[..start].copy_from_slice(self.head);
        into[start..end].fill(0);
        into[end..].copy_from_slice(self.tail);
        Ok(())
    }
    #[must_use]
    pub fn to_page(&self) -> Vec<u8> {
        let mut page = vec![0u8; PAGE_SIZE];
        let _ = self.restore(&mut page);
        page
    }
}
pub fn read_image_at(bytes: &[u8]) -> Result<(Image<'_>, usize), Error> {
    let prefix = bytes.get(..IMAGE_PREFIX_BYTES).ok_or(Error::Corrupt)?;
    let start = usize::from(u16::from_le_bytes([prefix[0], prefix[1]]));
    let end = usize::from(u16::from_le_bytes([prefix[2], prefix[3]]));
    if start > end || end > PAGE_SIZE {
        return Err(Error::Corrupt);
    }
    let len = image_len((start, end));
    let body = bytes.get(IMAGE_PREFIX_BYTES..len).ok_or(Error::Corrupt)?;
    let (head, tail) = body.split_at(start);
    if gap_of_header(head) != Some((start, end)) {
        return Err(Error::Corrupt);
    }
    Ok((Image { head, tail }, len))
}
pub fn read_image(bytes: &[u8]) -> Result<Image<'_>, Error> {
    let (image, len) = read_image_at(bytes)?;
    if len == bytes.len() { Ok(image) } else { Err(Error::Corrupt) }
}
pub const STAMPED: [(usize, usize); 2] = [
    (crate::node::offsets::CHECKSUM, crate::node::offsets::CHECKSUM + 4),
    (crate::node::offsets::GSN, crate::node::offsets::GSN + 8),
];
pub const OP_PREFIX_BYTES: usize = IMAGE_PREFIX_BYTES + 1 + 2 + 2 + 2;
pub const OP_INSERT: u8 = 1;
pub const OP_REPLACE: u8 = 2;
pub const OP_REMOVE: u8 = 3;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TupleOp<'a> {
    pub gap: (usize, usize),
    pub op: u8,
    pub flags: u16,
    pub key: &'a [u8],
    pub payload: &'a [u8],
}
#[must_use]
pub const fn op_len(key: &[u8], payload: &[u8]) -> usize {
    OP_PREFIX_BYTES + key.len() + payload.len()
}
fn op_is_canonical(op: &TupleOp<'_>) -> bool {
    let gap = op.gap;
    let gap_ok = gap.0 <= gap.1
        && gap.1 <= PAGE_SIZE
        && gap.0 >= crate::node::HEADER_BYTES
        && (gap.0 < gap.1 || gap == NO_GAP);
    let shape_ok = match op.op {
        OP_INSERT | OP_REPLACE => op.flags & !crate::node::flags::DEFINED == 0,
        OP_REMOVE => op.flags == 0 && op.payload.is_empty(),
        _ => false,
    };
    gap_ok && shape_ok && op.key.len() + op.payload.len() <= crate::node::DATA_BYTES
}
pub fn write_op(op: &TupleOp<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
    if !op_is_canonical(op) {
        return Err(Error::Corrupt);
    }
    let len16 = |n: usize| u16::try_from(n).map_err(|_| Error::Corrupt);
    out.clear();
    out.reserve(op_len(op.key, op.payload));
    out.extend_from_slice(&image_prefix(op.gap)?);
    out.push(op.op);
    out.extend_from_slice(&op.flags.to_le_bytes());
    out.extend_from_slice(&len16(op.key.len())?.to_le_bytes());
    out.extend_from_slice(&len16(op.payload.len())?.to_le_bytes());
    out.extend_from_slice(op.key);
    out.extend_from_slice(op.payload);
    Ok(())
}
pub fn read_op_at(bytes: &[u8]) -> Result<(TupleOp<'_>, usize), Error> {
    let prefix = bytes.get(..OP_PREFIX_BYTES).ok_or(Error::Corrupt)?;
    let word = |at: usize| u16::from_le_bytes([prefix[at], prefix[at + 1]]);
    let gap = (usize::from(word(0)), usize::from(word(2)));
    let (key_len, payload_len) = (usize::from(word(7)), usize::from(word(9)));
    let end = OP_PREFIX_BYTES + key_len + payload_len;
    let body = bytes.get(OP_PREFIX_BYTES..end).ok_or(Error::Corrupt)?;
    let (key, payload) = body.split_at(key_len);
    let op = TupleOp { gap, op: prefix[4], flags: word(5), key, payload };
    if !op_is_canonical(&op) {
        return Err(Error::Corrupt);
    }
    Ok((op, end))
}
pub fn read_op(bytes: &[u8]) -> Result<TupleOp<'_>, Error> {
    let (op, len) = read_op_at(bytes)?;
    if len == bytes.len() { Ok(op) } else { Err(Error::Corrupt) }
}
impl TupleOp<'_> {
    pub fn apply(&self, page: &mut [u8]) -> Result<(), Error> {
        use crate::node::{Node, PageKind};
        let bytes: &mut [u8; PAGE_SIZE] = page.try_into().map_err(|_| Error::Corrupt)?;
        let mut node = Node::from_page_bytes(bytes);
        let kind = node.kind();
        if !node.is_well_formed()
            || !(kind == PageKind::Leaf as u8 || kind == PageKind::Value as u8)
            || !node.covers(self.key)
        {
            return Err(Error::Corrupt);
        }
        let (at, present) = node.lower_bound(self.key);
        let done = match self.op {
            OP_INSERT if !present => node.insert_reporting(self.key, self.payload, self.flags).0,
            OP_REPLACE if present => node.insert_reporting(self.key, self.payload, self.flags).0,
            OP_REMOVE if present => node.remove_slot(at),
            _ => false,
        };
        let (count, top) = node.extent();
        let gap = gap_named(
            u16::try_from(count).map_err(|_| Error::Corrupt)?,
            u16::try_from(top).map_err(|_| Error::Corrupt)?,
        );
        if !done || !node.is_well_formed() || gap != self.gap {
            return Err(Error::Corrupt);
        }
        *bytes = node.to_page_bytes();
        bytes[gap.0..gap.1].fill(0);
        Ok(())
    }
}
pub const SMO_ENTRY_PREFIX_BYTES: usize = 16;
pub const SMO_ENTRY_MAX_BYTES: usize = SMO_ENTRY_PREFIX_BYTES + IMAGE_MAX_BYTES;
pub const SMO_MIN_PAGES: usize = 2;
pub const SMO_MAX_PAGES: usize = 4;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmoEntry<'a> {
    pub page: u64,
    pub prev: u64,
    pub image: &'a [u8],
}
#[must_use]
pub const fn smo_max_len(pages: usize) -> usize {
    pages * SMO_ENTRY_MAX_BYTES
}
#[must_use]
pub fn smo_len(entries: &[SmoEntry<'_>]) -> usize {
    entries.iter().map(|e| SMO_ENTRY_PREFIX_BYTES + e.image.len()).sum()
}
pub fn write_smo(entries: &[SmoEntry<'_>], into: &mut [u8]) -> Result<(), Error> {
    if entries.len() < SMO_MIN_PAGES || entries.len() > SMO_MAX_PAGES {
        return Err(Error::Corrupt);
    }
    if into.len() != smo_len(entries) {
        return Err(Error::Corrupt);
    }
    let mut at = 0;
    for entry in entries {
        read_image(entry.image)?;
        into[at..at + 8].copy_from_slice(&entry.page.to_le_bytes());
        into[at + 8..at + 16].copy_from_slice(&entry.prev.to_le_bytes());
        let from = at + SMO_ENTRY_PREFIX_BYTES;
        at = from + entry.image.len();
        into[from..at].copy_from_slice(entry.image);
    }
    Ok(())
}
fn smo_entry_at(payload: &[u8], from: usize) -> Result<(SmoEntry<'_>, usize), Error> {
    let fixed = payload.get(from..from + SMO_ENTRY_PREFIX_BYTES).ok_or(Error::Corrupt)?;
    let word = |o: usize| -> Result<u64, Error> {
        fixed[o..o + 8].try_into().map(u64::from_le_bytes).map_err(|_| Error::Corrupt)
    };
    let image_at = from + SMO_ENTRY_PREFIX_BYTES;
    let (_, len) = read_image_at(&payload[image_at..])?;
    let next = image_at + len;
    Ok((SmoEntry { page: word(0)?, prev: word(8)?, image: &payload[image_at..next] }, next))
}
pub fn read_smo(payload: &[u8], at: usize) -> Result<SmoEntry<'_>, Error> {
    if at >= validate_smo(payload)? {
        return Err(Error::Corrupt);
    }
    let mut from = 0;
    for _ in 0..at {
        from = smo_entry_at(payload, from)?.1;
    }
    Ok(smo_entry_at(payload, from)?.0)
}
pub fn smo_pages(payload: &[u8]) -> Result<usize, Error> {
    validate_smo(payload)
}
fn validate_smo(payload: &[u8]) -> Result<usize, Error> {
    let (mut from, mut pages) = (0, 0);
    while from < payload.len() {
        if pages == SMO_MAX_PAGES {
            return Err(Error::Corrupt);
        }
        from = smo_entry_at(payload, from)?.1;
        pages += 1;
    }
    if pages < SMO_MIN_PAGES {
        return Err(Error::Corrupt);
    }
    Ok(pages)
}
pub const VALUE_ENTRY_PREFIX_BYTES: usize = 17;
pub const FORM_IMAGE: u8 = 0;
pub const FORM_OP: u8 = 1;
pub const FORM_RUN: u8 = 2;
pub const FORM_RETIRE: u8 = 3;
pub const RUN_BODY_BYTES: usize = 8;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change<'a> {
    Image(&'a [u8]),
    Op(&'a [u8]),
    Run(&'a [u8]),
    Retire(&'a [u8]),
}
impl<'a> Change<'a> {
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        match self {
            Self::Image(b) | Self::Op(b) | Self::Run(b) | Self::Retire(b) => b,
        }
    }
    #[must_use]
    pub const fn is_run(&self) -> bool {
        matches!(self, Self::Run(_) | Self::Retire(_))
    }
    #[must_use]
    pub const fn is_retire(&self) -> bool {
        matches!(self, Self::Retire(_))
    }
    #[must_use]
    pub const fn is_op(&self) -> bool {
        matches!(self, Self::Op(_))
    }
    pub fn rebuild(&self, page: &mut [u8]) -> Result<(), Error> {
        match self {
            Self::Image(b) => read_image(b)?.restore(page),
            Self::Op(b) => read_op(b)?.apply(page),
            Self::Run(_) | Self::Retire(_) => Err(Error::Corrupt),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueEntry<'a> {
    pub page: u64,
    pub prev: u64,
    pub change: Change<'a>,
}
#[must_use]
pub fn value_write_len(entries: &[ValueEntry<'_>]) -> usize {
    entries.iter().map(|e| VALUE_ENTRY_PREFIX_BYTES + e.change.bytes().len()).sum()
}
pub fn write_value_write(entries: &[ValueEntry<'_>], into: &mut [u8]) -> Result<(), Error> {
    if entries.len() < SMO_MIN_PAGES
        || entries.len() > SMO_MAX_PAGES
        || into.len() != value_write_len(entries)
    {
        return Err(Error::Corrupt);
    }
    write_entries(entries, into)
}
fn write_entries(entries: &[ValueEntry<'_>], into: &mut [u8]) -> Result<(), Error> {
    if into.len() != value_write_len(entries) {
        return Err(Error::Corrupt);
    }
    let mut at = 0;
    for entry in entries {
        let form = match entry.change {
            Change::Image(b) => {
                read_image(b)?;
                FORM_IMAGE
            }
            Change::Op(b) => {
                read_op(b)?;
                if entry.prev == 0 {
                    return Err(Error::Corrupt);
                }
                FORM_OP
            }
            Change::Run(b) | Change::Retire(b) => {
                if entry.prev == 0 || entry.page == 0 || b.len() != RUN_BODY_BYTES {
                    return Err(Error::Corrupt);
                }
                if entry.change.is_retire() { FORM_RETIRE } else { FORM_RUN }
            }
        };
        into[at..at + 8].copy_from_slice(&entry.page.to_le_bytes());
        into[at + 8..at + 16].copy_from_slice(&entry.prev.to_le_bytes());
        into[at + 16] = form;
        let from = at + VALUE_ENTRY_PREFIX_BYTES;
        let bytes = entry.change.bytes();
        at = from + bytes.len();
        into[from..at].copy_from_slice(bytes);
    }
    Ok(())
}
fn value_entry_at(payload: &[u8], from: usize) -> Result<(ValueEntry<'_>, usize), Error> {
    let fixed = payload.get(from..from + VALUE_ENTRY_PREFIX_BYTES).ok_or(Error::Corrupt)?;
    let word = |o: usize| -> Result<u64, Error> {
        fixed[o..o + 8].try_into().map(u64::from_le_bytes).map_err(|_| Error::Corrupt)
    };
    let (page, prev) = (word(0)?, word(8)?);
    let body_at = from + VALUE_ENTRY_PREFIX_BYTES;
    let rest = &payload[body_at..];
    let (change, len) = match fixed[16] {
        FORM_IMAGE => {
            let (_, len) = read_image_at(rest)?;
            (Change::Image(&rest[..len]), len)
        }
        FORM_OP if prev != 0 => {
            let (_, len) = read_op_at(rest)?;
            (Change::Op(&rest[..len]), len)
        }
        FORM_RUN if prev != 0 && page != 0 && rest.len() >= RUN_BODY_BYTES => {
            (Change::Run(&rest[..RUN_BODY_BYTES]), RUN_BODY_BYTES)
        }
        FORM_RETIRE if prev != 0 && page != 0 && rest.len() >= RUN_BODY_BYTES => {
            (Change::Retire(&rest[..RUN_BODY_BYTES]), RUN_BODY_BYTES)
        }
        _ => return Err(Error::Corrupt),
    };
    Ok((ValueEntry { page, prev, change }, body_at + len))
}
fn validate_value_write(payload: &[u8]) -> Result<usize, Error> {
    let (mut from, mut pages) = (0, 0);
    while from < payload.len() {
        if pages == SMO_MAX_PAGES {
            return Err(Error::Corrupt);
        }
        from = value_entry_at(payload, from)?.1;
        pages += 1;
    }
    if pages < SMO_MIN_PAGES {
        return Err(Error::Corrupt);
    }
    Ok(pages)
}
pub fn value_write_pages(payload: &[u8]) -> Result<usize, Error> {
    validate_value_write(payload)
}
pub fn read_value_write(payload: &[u8], at: usize) -> Result<ValueEntry<'_>, Error> {
    if at >= validate_value_write(payload)? {
        return Err(Error::Corrupt);
    }
    let mut from = 0;
    for _ in 0..at {
        from = value_entry_at(payload, from)?.1;
    }
    Ok(value_entry_at(payload, from)?.0)
}
pub const UNDO_PREFIX_BYTES: usize = 1 + 2 + 2 + UNDO_HEAD_BYTES;
pub const UNDO_HEAD_BYTES: usize = crate::txn::tuple::CHAINED_HEADER_BYTES;
const UNDO_HAS_BEFORE: u8 = 0b0000_0001;
pub const CHANGE_MIN_PAGES: usize = 1;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoPart<'a> {
    pub key: &'a [u8],
    pub head: &'a [u8],
    pub before: Option<&'a [u8]>,
}
pub fn undo_part_len(undo: &UndoPart<'_>) -> Result<usize, Error> {
    let before = undo.before.map_or(0, <[u8]>::len);
    if undo.key.len() > usize::from(u16::MAX)
        || before > usize::from(u16::MAX)
        || undo.head.len() != UNDO_HEAD_BYTES
    {
        return Err(Error::Corrupt);
    }
    Ok(UNDO_PREFIX_BYTES + undo.key.len() + before)
}
pub fn write_undo_part(undo: &UndoPart<'_>, into: &mut [u8]) -> Result<(), Error> {
    if into.len() != undo_part_len(undo)? {
        return Err(Error::Corrupt);
    }
    let len16 = |n: usize| u16::try_from(n).map_err(|_| Error::Corrupt);
    into[0] = if undo.before.is_some() { UNDO_HAS_BEFORE } else { 0 };
    into[1..3].copy_from_slice(&len16(undo.key.len())?.to_le_bytes());
    into[3..5].copy_from_slice(&len16(undo.before.map_or(0, <[u8]>::len))?.to_le_bytes());
    into[5..UNDO_PREFIX_BYTES].copy_from_slice(undo.head);
    let at = UNDO_PREFIX_BYTES + undo.key.len();
    into[UNDO_PREFIX_BYTES..at].copy_from_slice(undo.key);
    if let Some(before) = undo.before {
        into[at..].copy_from_slice(before);
    }
    Ok(())
}
pub fn read_undo_part(bytes: &[u8]) -> Result<(UndoPart<'_>, usize), Error> {
    let prefix = bytes.get(..UNDO_PREFIX_BYTES).ok_or(Error::Corrupt)?;
    let flags = prefix[0];
    let key_len = usize::from(u16::from_le_bytes([prefix[1], prefix[2]]));
    let before_len = usize::from(u16::from_le_bytes([prefix[3], prefix[4]]));
    if flags & !UNDO_HAS_BEFORE != 0 || (before_len != 0 && flags & UNDO_HAS_BEFORE == 0) {
        return Err(Error::Corrupt);
    }
    let key_at = UNDO_PREFIX_BYTES;
    let before_at = key_at + key_len;
    let end = before_at + before_len;
    let body = bytes.get(..end).ok_or(Error::Corrupt)?;
    Ok((
        UndoPart {
            key: &body[key_at..before_at],
            head: &body[5..UNDO_PREFIX_BYTES],
            before: (flags & UNDO_HAS_BEFORE != 0).then(|| &body[before_at..end]),
        },
        end,
    ))
}
pub fn change_len(undo: &UndoPart<'_>, entries: &[ValueEntry<'_>]) -> Result<usize, Error> {
    Ok(undo_part_len(undo)? + value_write_len(entries))
}
pub fn write_change(
    undo: &UndoPart<'_>,
    entries: &[ValueEntry<'_>],
    into: &mut [u8],
) -> Result<(), Error> {
    if entries.len() < CHANGE_MIN_PAGES
        || entries.len() > SMO_MAX_PAGES
        || into.len() != change_len(undo, entries)?
    {
        return Err(Error::Corrupt);
    }
    let (head, rest) = into.split_at_mut(undo_part_len(undo)?);
    write_undo_part(undo, head)?;
    write_entries(entries, rest)
}
fn validate_change(payload: &[u8]) -> Result<(UndoPart<'_>, usize, usize), Error> {
    let (undo, entries_at) = read_undo_part(payload)?;
    let (mut from, mut pages) = (entries_at, 0);
    while from < payload.len() {
        if pages == SMO_MAX_PAGES {
            return Err(Error::Corrupt);
        }
        from = value_entry_at(payload, from)?.1;
        pages += 1;
    }
    if pages < CHANGE_MIN_PAGES {
        return Err(Error::Corrupt);
    }
    Ok((undo, entries_at, pages))
}
pub fn read_change_undo(payload: &[u8]) -> Result<UndoPart<'_>, Error> {
    validate_change(payload).map(|(undo, _, _)| undo)
}
pub fn change_pages(payload: &[u8]) -> Result<usize, Error> {
    validate_change(payload).map(|(_, _, pages)| pages)
}
pub fn read_change(payload: &[u8], at: usize) -> Result<ValueEntry<'_>, Error> {
    let (_, mut from, pages) = validate_change(payload)?;
    if at >= pages {
        return Err(Error::Corrupt);
    }
    for _ in 0..at {
        from = value_entry_at(payload, from)?.1;
    }
    Ok(value_entry_at(payload, from)?.0)
}
pub fn read_commit_remote(payload: &[u8]) -> Result<bool, Error> {
    read_commit(payload).map(|(remote, _)| remote)
}
#[must_use]
pub const fn commit_remote(remote: bool) -> [u8; 1] {
    [remote as u8]
}
pub const NAMED_DEP_BYTES: usize = 2 + 8 + 8;
pub fn commit_payload(
    remote: bool,
    named: &[(u16, u64, u64)],
    into: &mut Vec<u8>,
) -> Result<(), Error> {
    into.clear();
    into.push(u8::from(remote));
    if named.is_empty() {
        return Ok(());
    }
    let count = u16::try_from(named.len())
        .map_err(|_| Error::TooManyDependencies { named: named.len() })?;
    into.extend_from_slice(&count.to_le_bytes());
    for &(worker, tag, upto) in named {
        into.extend_from_slice(&worker.to_le_bytes());
        into.extend_from_slice(&tag.to_le_bytes());
        into.extend_from_slice(&upto.to_le_bytes());
    }
    Ok(())
}
pub fn read_commit(payload: &[u8]) -> Result<(bool, Vec<(u16, u64, u64)>), Error> {
    let (remote, rest) = match payload.split_first() {
        Some((0, rest)) => (false, rest),
        Some((1, rest)) => (true, rest),
        _ => return Err(Error::Corrupt),
    };
    if rest.is_empty() {
        return Ok((remote, Vec::new()));
    }
    let (count, deps) = rest.split_at_checked(2).ok_or(Error::Corrupt)?;
    let count = usize::from(u16::from_le_bytes([count[0], count[1]]));
    if count == 0 || deps.len() != count * NAMED_DEP_BYTES {
        return Err(Error::Corrupt);
    }
    let named = deps
        .as_chunks::<NAMED_DEP_BYTES>()
        .0
        .iter()
        .map(|d| {
            let worker = u16::from_le_bytes([d[0], d[1]]);
            let tag = u64::from_le_bytes(d[2..10].try_into().unwrap_or_default());
            let upto = u64::from_le_bytes(d[10..18].try_into().unwrap_or_default());
            (worker, tag, upto)
        })
        .collect();
    Ok((remote, named))
}
pub fn validate(kind: Kind, payload: &[u8]) -> Result<(), Error> {
    match kind {
        Kind::Smo => validate_smo(payload).map(|_| ()),
        Kind::ValueWrite => validate_value_write(payload).map(|_| ()),
        Kind::PageOp => read_op(payload).map(|_| ()),
        Kind::Change => validate_change(payload).map(|_| ()),
        Kind::Commit => read_commit_remote(payload).map(|_| ()),
        Kind::Page => read_image(payload).map(|_| ()),
        Kind::Start | Kind::Abort => {
            if payload.is_empty() {
                Ok(())
            } else {
                Err(Error::Corrupt)
            }
        }
        Kind::CarriageReturn => Ok(()),
    }
}
