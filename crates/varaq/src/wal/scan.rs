use crate::Error;
use crate::wal::record::{self, Header, Kind};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stop {
    TooShort,
    NotARecord,
    StaleLap {
        expected: u64,
        found: u64,
    },
    GsnWentBack {
        previous: u64,
        found: u64,
    },
    #[default]
    Exhausted,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    pub records: Vec<Header>,
    pub stop: Stop,
    pub consumed: usize,
}
#[must_use]
pub fn scan(bytes: &[u8], lap: u64, from_gsn: u64) -> Scanned {
    let mut records = Vec::new();
    let mut at = 0usize;
    let mut previous = from_gsn;
    loop {
        if at + record::HEADER_BYTES > bytes.len() {
            return Scanned { records, stop: Stop::TooShort, consumed: at };
        }
        let Ok(found) = record::decode(&bytes[at..]) else {
            return Scanned { records, stop: Stop::NotARecord, consumed: at };
        };
        let header = found.header;
        if header.lap != lap {
            let stop = Stop::StaleLap { expected: lap, found: header.lap };
            return Scanned { records, stop, consumed: at };
        }
        if header.kind != Kind::CarriageReturn && header.gsn <= previous {
            let stop = Stop::GsnWentBack { previous, found: header.gsn };
            return Scanned { records, stop, consumed: at };
        }
        if header.kind != Kind::CarriageReturn {
            previous = header.gsn;
        }
        at += header.total_len();
        records.push(header);
        if at == bytes.len() {
            return Scanned { records, stop: Stop::Exhausted, consumed: at };
        }
    }
}
#[must_use]
pub fn is_clean_end(stop: Stop) -> bool {
    matches!(stop, Stop::TooShort | Stop::Exhausted)
}
pub type Source<'r> = &'r dyn Fn(u64, &mut [u8]) -> Result<(), Error>;
pub struct Window<'r> {
    read: Source<'r>,
    len: u64,
    start: u64,
    filled: usize,
    buf: Vec<u8>,
    size: usize,
}
impl<'r> Window<'r> {
    #[must_use]
    pub fn new(len: u64, size: usize, read: Source<'r>) -> Self {
        Self { read, len, start: 0, filled: 0, buf: Vec::new(), size: size.max(crate::PAGE_SIZE) }
    }
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn view(&mut self, at: u64, end: u64, want: usize) -> Result<&[u8], Error> {
        if at > end || end > self.len {
            return Err(Error::Corrupt);
        }
        let need = end.min(at.saturating_add(want as u64));
        let held = self.start + self.filled as u64;
        if at < self.start || need > held {
            let page = crate::PAGE_SIZE as u64;
            let from = at - at % page;
            let span = (need - from).max(self.size as u64).div_ceil(page) * page;
            let n = usize::try_from(span.min(self.len - from)).map_err(|_| Error::Corrupt)?;
            if self.buf.len() < n {
                self.buf.resize(n, 0);
            }
            (self.read)(from, &mut self.buf[..n])?;
            self.start = from;
            self.filled = n;
        }
        let lo = usize::try_from(at - self.start).map_err(|_| Error::Corrupt)?;
        let hi = usize::try_from(end.min(self.start + self.filled as u64) - self.start)
            .map_err(|_| Error::Corrupt)?;
        Ok(&self.buf[lo..hi])
    }
    pub fn record(&mut self, at: u64, end: u64) -> Result<Option<(Header, &[u8])>, Error> {
        let want = {
            let head = self.view(at, end, record::HEADER_BYTES)?;
            whole_len(head).unwrap_or(record::HEADER_BYTES)
        };
        let bytes = self.view(at, end, want)?;
        Ok(record::decode(bytes).ok().map(|r| (r.header, r.payload)))
    }
}
fn whole_len(head: &[u8]) -> Option<usize> {
    let raw = head.get(record::offsets::LEN..record::offsets::LEN + 4)?;
    let len = u32::from_le_bytes(raw.try_into().ok()?) as usize;
    (len <= record::MAX_PAYLOAD).then_some(record::HEADER_BYTES + len)
}
pub fn scan_window(
    window: &mut Window<'_>,
    from: u64,
    end: u64,
    lap: u64,
    from_gsn: u64,
    visit: &mut dyn FnMut(&Header, u64, &[u8]),
) -> Result<(Stop, u64), Error> {
    let mut at = from;
    let mut previous = from_gsn;
    let mut want = record::HEADER_BYTES;
    loop {
        let bytes = window.view(at, end, want)?;
        let found = scan(bytes, lap, previous);
        let mut offset = 0usize;
        for header in &found.records {
            let total = header.total_len();
            visit(
                header,
                at + offset as u64,
                &bytes[offset + record::HEADER_BYTES..offset + total],
            );
            if header.kind != Kind::CarriageReturn {
                previous = header.gsn;
            }
            offset += total;
        }
        let at_end = at + bytes.len() as u64 == end;
        let rest = &bytes[found.consumed..];
        at += found.consumed as u64;
        let short = match found.stop {
            Stop::Exhausted | Stop::TooShort => Some(record::HEADER_BYTES),
            Stop::NotARecord => whole_len(rest).filter(|&n| n > rest.len()),
            Stop::StaleLap { .. } | Stop::GsnWentBack { .. } => None,
        };
        match short {
            Some(n) if !at_end => want = n,
            _ => return Ok((found.stop, at - from)),
        }
    }
}
pub fn highest_stamp_on_device(window: &mut Window<'_>) -> Result<u64, Error> {
    let page = crate::PAGE_SIZE as u64;
    let len = window.len();
    let mut highest = 0;
    let mut covered = 0u64;
    let mut start = 0u64;
    while start < len {
        let mut at = start.max(covered);
        if at == start {
            while at + record::HEADER_BYTES as u64 <= len {
                let Some((header, _)) = window.record(at, len)? else { break };
                highest = highest.max(header.gsn);
                at += header.total_len() as u64;
            }
            covered = at;
        }
        start += page;
    }
    Ok(highest)
}
pub fn require_clean_end(scanned: &Scanned) -> Result<(), Error> {
    if is_clean_end(scanned.stop) { Ok(()) } else { Err(Error::Corrupt) }
}
