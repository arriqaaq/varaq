use crate::Error;
use crate::wal::record::{self, Header, Kind};
#[derive(Debug)]
pub struct Ring {
    buf: Box<[u8]>,
    log: u64,
    scratch: Vec<u8>,
    written: u64,
    flushed: u64,
    last_gsn: u64,
    retain: Option<u64>,
    unit: u64,
}
impl Ring {
    pub fn with_capacity(capacity: usize) -> Result<Self, Error> {
        Self::staged(capacity, capacity)
    }
    pub fn staged(memory: usize, device: usize) -> Result<Self, Error> {
        if memory < record::HEADER_BYTES || device < memory || !device.is_multiple_of(memory) {
            return Err(Error::Corrupt);
        }
        Ok(Self {
            buf: vec![0u8; memory].into_boxed_slice(),
            log: device as u64,
            scratch: Vec::new(),
            written: 0,
            flushed: 0,
            last_gsn: 0,
            retain: None,
            unit: 1,
        })
    }
    pub fn rewrites_in(&mut self, unit: usize) {
        self.unit = (unit as u64).max(1);
    }
    pub fn retain_everything(&mut self) {
        self.retain = Some(self.flushed);
    }
    pub fn release_before(&mut self, at: u64) -> Result<(), Error> {
        if at > self.flushed {
            return Err(Error::Corrupt);
        }
        if let Some(r) = self.retain {
            self.retain = Some(r.max(at));
        }
        Ok(())
    }
    #[must_use]
    pub fn retained_from(&self) -> Option<u64> {
        self.retain
    }
    pub fn resume(
        &mut self,
        read: crate::wal::scan::Source<'_>,
        at: u64,
        last_gsn: u64,
        begin: u64,
    ) -> Result<(), Error> {
        if begin > at {
            return Err(Error::Corrupt);
        }
        let mut o = at - at.min(self.buf.len() as u64);
        while o < at {
            let d = o % self.log;
            let m = usize::try_from(o % self.buf.len() as u64).map_err(|_| Error::Corrupt)?;
            let n = usize::try_from((at - o).min(self.log - d))
                .unwrap_or(usize::MAX)
                .min(self.buf.len() - m);
            read(d, &mut self.buf[m..m + n])?;
            o += n as u64;
        }
        self.written = at;
        self.flushed = at;
        self.last_gsn = last_gsn;
        self.retain = Some(begin);
        Ok(())
    }
    #[must_use]
    pub fn capacity(&self) -> usize {
        usize::try_from(self.log).unwrap_or(usize::MAX)
    }
    #[must_use]
    pub fn buffer(&self) -> usize {
        self.buf.len()
    }
    #[must_use]
    pub fn lap(&self) -> u64 {
        self.written / self.capacity() as u64
    }
    #[must_use]
    pub fn last_gsn(&self) -> u64 {
        self.last_gsn
    }
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }
    #[must_use]
    pub fn flushed(&self) -> u64 {
        self.flushed
    }
    #[must_use]
    pub fn pending(&self) -> usize {
        usize::try_from(self.written - self.flushed).unwrap_or(usize::MAX)
    }
    #[must_use]
    pub fn retains(&self) -> bool {
        self.retain.is_some()
    }
    #[must_use]
    pub fn free(&self) -> usize {
        let keep = self.retain.map_or(self.flushed, |r| r.min(self.flushed));
        let free = crate::calc::room(self.written, keep, self.log, self.unit);
        usize::try_from(free).unwrap_or(usize::MAX)
    }
    #[must_use]
    pub fn buffer_free(&self) -> usize {
        let free = crate::calc::room(self.written, self.flushed, self.buf.len() as u64, self.unit);
        usize::try_from(free).unwrap_or(usize::MAX)
    }
    fn zero(&mut self, at: u64, n: usize) {
        let m = self.buf.len();
        let i = usize::try_from(at % m as u64).unwrap_or(0);
        let first = (m - i).min(n);
        self.buf[i..i + first].fill(0);
        self.buf[..n - first].fill(0);
    }
    fn put(&mut self, at: u64, bytes: &[u8]) {
        let m = self.buf.len();
        let i = usize::try_from(at % m as u64).unwrap_or(0);
        let first = (m - i).min(bytes.len());
        self.buf[i..i + first].copy_from_slice(&bytes[..first]);
        self.buf[..bytes.len() - first].copy_from_slice(&bytes[first..]);
    }
    pub fn append(&mut self, mut header: Header, payload: &[u8]) -> Result<u64, Error> {
        let len = header.total_len();
        let capacity = self.capacity();
        let to_usize = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        let keep = self.retain.map_or(self.flushed, |r| r.min(self.flushed));
        let placed = crate::calc::place(
            self.written,
            keep,
            self.flushed,
            self.log,
            self.buf.len() as u64,
            self.unit,
            len as u64,
        );
        let start = match placed {
            crate::calc::Placement::Fits { at, .. } => at,
            crate::calc::Placement::DeviceFull { needed, free } => {
                return Err(Error::LogFull {
                    capacity,
                    needed: to_usize(needed),
                    free: to_usize(free),
                });
            }
            crate::calc::Placement::MemoryFull { needed, free } => {
                return Err(Error::LogFull {
                    capacity: self.buffer(),
                    needed: to_usize(needed),
                    free: to_usize(free),
                });
            }
        };
        if start > self.written {
            let to_end = to_usize(start - self.written);
            if to_end >= record::HEADER_BYTES {
                let pad = to_end - record::HEADER_BYTES;
                let filler = Header {
                    prev: 0,
                    lap: self.written / capacity as u64,
                    kind: Kind::CarriageReturn,
                    worker: header.worker,
                    gsn: header.gsn,
                    page: 0,
                    txn: 0,
                    len: u32::try_from(pad).map_err(|_| Error::Corrupt)?,
                };
                let mut bytes = Vec::with_capacity(to_end);
                record::encode(filler, &vec![0u8; pad], &mut bytes)?;
                self.put(self.written, &bytes);
            } else {
                self.zero(self.written, to_end);
            }
            self.written = start;
        }
        let at = self.written;
        header.lap = at / capacity as u64;
        let offset = usize::try_from(at % self.buf.len() as u64).unwrap_or(0);
        if offset + len <= self.buf.len() {
            record::encode_into(header, payload, &mut self.buf[offset..offset + len])?;
        } else {
            let mut scratch = core::mem::take(&mut self.scratch);
            scratch.clear();
            scratch.resize(len, 0);
            let encoded = record::encode_into(header, payload, &mut scratch);
            if encoded.is_ok() {
                self.put(at, &scratch);
            }
            self.scratch = scratch;
            encoded?;
        }
        self.written += len as u64;
        self.last_gsn = self.last_gsn.max(header.gsn);
        Ok(at)
    }
    #[must_use]
    pub fn pending_spans(&self) -> (&[u8], &[u8]) {
        self.spans_from(self.flushed)
    }
    #[must_use]
    pub fn spans_from(&self, at: u64) -> (&[u8], &[u8]) {
        let capacity = self.buffer();
        let pending = usize::try_from(self.written.saturating_sub(at)).unwrap_or(0);
        if pending == 0 {
            return (&[], &[]);
        }
        let from = usize::try_from(at % capacity as u64).unwrap_or(0);
        let to_end = capacity - from;
        if pending <= to_end {
            (&self.buf[from..from + pending], &[])
        } else {
            (&self.buf[from..], &self.buf[..pending - to_end])
        }
    }
    pub fn mark_flushed(&mut self, upto: u64) -> Result<(), Error> {
        if upto > self.written || upto < self.flushed {
            return Err(Error::Corrupt);
        }
        self.flushed = upto;
        Ok(())
    }
}
