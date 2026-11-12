use crate::Error;
use crate::page::checksum;
pub mod payload;
pub const HEADER_BYTES: usize = 52;
pub const MAX_PAYLOAD: usize = 4 << 20;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Start = 1,
    Commit = 2,
    Abort = 3,
    Page = 9,
    CarriageReturn = 5,
    Smo = 10,
    ValueWrite = 15,
    PageOp = 14,
    Change = 16,
}
impl Kind {
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Start),
            2 => Some(Self::Commit),
            3 => Some(Self::Abort),
            5 => Some(Self::CarriageReturn),
            9 => Some(Self::Page),
            10 => Some(Self::Smo),
            14 => Some(Self::PageOp),
            15 => Some(Self::ValueWrite),
            16 => Some(Self::Change),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub worker: u16,
    pub gsn: u64,
    pub page: u64,
    pub txn: u64,
    pub len: u32,
    pub prev: u64,
    pub lap: u64,
}
impl Header {
    #[must_use]
    pub const fn total_len(&self) -> usize {
        HEADER_BYTES + self.len as usize
    }
}
pub mod offsets {
    pub const CHECKSUM: usize = 0;
    pub const KIND: usize = 4;
    pub const WORKER: usize = 5;
    pub const PADDING: usize = 7;
    pub const LEN: usize = 8;
    pub const GSN: usize = 12;
    pub const PAGE: usize = 20;
    pub const TXN: usize = 28;
    pub const PREV: usize = 36;
    pub const LAP: usize = 44;
}
const COVERED_FROM: usize = 4;
fn checksum_of(record: &[u8]) -> u32 {
    checksum::crc32c(&record[COVERED_FROM..])
}
pub fn encode(header: Header, payload: &[u8], into: &mut Vec<u8>) -> Result<(), Error> {
    let start = into.len();
    into.resize(start + header.total_len(), 0);
    if let Err(refused) = encode_into(header, payload, &mut into[start..]) {
        into.truncate(start);
        return Err(refused);
    }
    Ok(())
}
pub fn encode_into(header: Header, payload: &[u8], into: &mut [u8]) -> Result<(), Error> {
    if payload.len() > MAX_PAYLOAD || payload.len() != header.len as usize {
        return Err(Error::Corrupt);
    }
    if into.len() != header.total_len() {
        return Err(Error::Corrupt);
    }
    payload::validate(header.kind, payload)?;
    write_record(header, payload, into);
    Ok(())
}
fn write_record(header: Header, payload: &[u8], into: &mut [u8]) {
    into[offsets::CHECKSUM..offsets::CHECKSUM + 4].copy_from_slice(&0u32.to_le_bytes());
    into[offsets::KIND] = header.kind as u8;
    into[offsets::WORKER..offsets::WORKER + 2].copy_from_slice(&header.worker.to_le_bytes());
    into[offsets::PADDING] = 0;
    into[offsets::LEN..offsets::LEN + 4].copy_from_slice(&header.len.to_le_bytes());
    into[offsets::GSN..offsets::GSN + 8].copy_from_slice(&header.gsn.to_le_bytes());
    into[offsets::PAGE..offsets::PAGE + 8].copy_from_slice(&header.page.to_le_bytes());
    into[offsets::TXN..offsets::TXN + 8].copy_from_slice(&header.txn.to_le_bytes());
    into[offsets::PREV..offsets::PREV + 8].copy_from_slice(&header.prev.to_le_bytes());
    into[offsets::LAP..offsets::LAP + 8].copy_from_slice(&header.lap.to_le_bytes());
    into[HEADER_BYTES..HEADER_BYTES + payload.len()].copy_from_slice(payload);
    let crc = checksum_of(into);
    into[offsets::CHECKSUM..offsets::CHECKSUM + 4].copy_from_slice(&crc.to_le_bytes());
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    pub header: Header,
    pub payload: &'a [u8],
}
pub fn decode(bytes: &[u8]) -> Result<Record<'_>, Error> {
    if bytes.len() < HEADER_BYTES {
        return Err(Error::Corrupt);
    }
    let Some(kind) = Kind::from_u8(bytes[offsets::KIND]) else { return Err(Error::Corrupt) };
    if bytes[offsets::PADDING] != 0 {
        return Err(Error::Corrupt);
    }
    let worker = u16::from_le_bytes([bytes[offsets::WORKER], bytes[offsets::WORKER + 1]]);
    let word = |at: usize| -> Result<u64, Error> {
        bytes[at..at + 8].try_into().map(u64::from_le_bytes).map_err(|_| Error::Corrupt)
    };
    let len = u32::from_le_bytes(
        bytes[offsets::LEN..offsets::LEN + 4].try_into().map_err(|_| Error::Corrupt)?,
    );
    if len as usize > MAX_PAYLOAD {
        return Err(Error::Corrupt);
    }
    let header = Header {
        kind,
        worker,
        gsn: word(offsets::GSN)?,
        page: word(offsets::PAGE)?,
        txn: word(offsets::TXN)?,
        len,
        prev: word(offsets::PREV)?,
        lap: word(offsets::LAP)?,
    };
    let total = header.total_len();
    if bytes.len() < total {
        return Err(Error::Corrupt);
    }
    let stored = u32::from_le_bytes(
        bytes[offsets::CHECKSUM..offsets::CHECKSUM + 4].try_into().map_err(|_| Error::Corrupt)?,
    );
    if stored != checksum_of(&bytes[..total]) {
        return Err(Error::Corrupt);
    }
    let payload = &bytes[HEADER_BYTES..total];
    payload::validate(kind, payload)?;
    if kind == Kind::PageOp && header.prev == 0 {
        return Err(Error::Corrupt);
    }
    if kind == Kind::Change && (header.page != 0 || header.prev != 0) {
        return Err(Error::Corrupt);
    }
    Ok(Record { header, payload })
}
