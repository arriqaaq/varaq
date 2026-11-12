use crate::Error;
pub const HEADER_BYTES: usize = 1;
pub const CHAINED_HEADER_BYTES: usize = 1 + 2 + 8 + 8;
pub const HEAD_BYTES: usize = 1 + 2 + 8;
const _: () = assert!(HEAD_BYTES == crate::node::separated::LOCAL_MOST);
const VISIBLE_FOR_ALL: u8 = 0;
const CHAINED: u8 = 1;
const CHAINED_REMOVED: u8 = 2;
pub const NO_PREVIOUS: u64 = 0;
pub fn encode_visible_for_all(payload: &[u8], into: &mut Vec<u8>) {
    into.clear();
    into.reserve(HEADER_BYTES + payload.len());
    into.push(VISIBLE_FOR_ALL);
    into.extend_from_slice(payload);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub writer: u16,
    pub tx_ts: u64,
    pub previous: u64,
}
pub fn encode_removed(header: Version, into: &mut Vec<u8>) {
    into.clear();
    into.reserve(CHAINED_HEADER_BYTES);
    into.push(CHAINED_REMOVED);
    into.extend_from_slice(&header.writer.to_le_bytes());
    into.extend_from_slice(&header.tx_ts.to_le_bytes());
    into.extend_from_slice(&header.previous.to_le_bytes());
}
pub fn encode_chained(header: Version, payload: &[u8], into: &mut Vec<u8>) {
    into.clear();
    into.reserve(CHAINED_HEADER_BYTES + payload.len());
    into.push(CHAINED);
    into.extend_from_slice(&header.writer.to_le_bytes());
    into.extend_from_slice(&header.tx_ts.to_le_bytes());
    into.extend_from_slice(&header.previous.to_le_bytes());
    into.extend_from_slice(payload);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored<'a> {
    VisibleForAll(&'a [u8]),
    Chained(Version, &'a [u8]),
    Removed(Version),
}
impl<'a> Stored<'a> {
    #[must_use]
    pub fn version(self) -> Option<Version> {
        match self {
            Self::VisibleForAll(_) => None,
            Self::Chained(v, _) | Self::Removed(v) => Some(v),
        }
    }
    #[must_use]
    pub fn payload(self) -> Option<&'a [u8]> {
        match self {
            Self::VisibleForAll(p) | Self::Chained(_, p) => Some(p),
            Self::Removed(_) => None,
        }
    }
}
fn chained_header(stored: &[u8]) -> Result<Version, Error> {
    if stored.len() < CHAINED_HEADER_BYTES {
        return Err(Error::Corrupt);
    }
    let writer = u16::from_le_bytes([stored[1], stored[2]]);
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&stored[3..11]);
    let mut prev = [0u8; 8];
    prev.copy_from_slice(&stored[11..19]);
    Ok(Version { writer, tx_ts: u64::from_le_bytes(ts), previous: u64::from_le_bytes(prev) })
}
pub fn decode(stored: &[u8]) -> Result<Stored<'_>, Error> {
    match stored.first() {
        Some(&VISIBLE_FOR_ALL) => Ok(Stored::VisibleForAll(&stored[HEADER_BYTES..])),
        Some(&tag @ (CHAINED | CHAINED_REMOVED)) => {
            let removed = tag == CHAINED_REMOVED;
            if !version_len_fits(removed, stored.len()) {
                return Err(Error::Corrupt);
            }
            let version = chained_header(stored)?;
            Ok(if removed {
                Stored::Removed(version)
            } else {
                Stored::Chained(version, &stored[CHAINED_HEADER_BYTES..])
            })
        }
        Some(_) | None => Err(Error::Corrupt),
    }
}
#[must_use]
pub const fn version_len_fits(removed: bool, len: usize) -> bool {
    if removed { len == CHAINED_HEADER_BYTES } else { len >= CHAINED_HEADER_BYTES }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    VisibleForAll,
    Chained { writer: u16, tx_ts: u64 },
    Removed { writer: u16, tx_ts: u64 },
}
pub fn decode_head(bytes: &[u8]) -> Result<Option<Head>, Error> {
    let version = |bytes: &[u8]| {
        if bytes.len() < HEAD_BYTES {
            return None;
        }
        let writer = u16::from_le_bytes([bytes[1], bytes[2]]);
        let mut ts = [0_u8; 8];
        ts.copy_from_slice(&bytes[3..11]);
        Some((writer, u64::from_le_bytes(ts)))
    };
    match bytes.first() {
        None => Ok(None),
        Some(&VISIBLE_FOR_ALL) => Ok(Some(Head::VisibleForAll)),
        Some(&CHAINED) => Ok(version(bytes).map(|(writer, tx_ts)| Head::Chained { writer, tx_ts })),
        Some(&CHAINED_REMOVED) => {
            Ok(version(bytes).map(|(writer, tx_ts)| Head::Removed { writer, tx_ts }))
        }
        Some(_) => Err(Error::Corrupt),
    }
}
pub fn head_of_stored(stored: &[u8]) -> Result<Option<Head>, Error> {
    match crate::node::blob::of_token(stored) {
        Some((_, kept)) => decode_head(kept),
        None => decode_head(stored),
    }
}
