use crate::Error;
use crate::wal::record::{Header, Kind, payload};
use crate::wal::worker::{Wal, current};
use std::collections::VecDeque;
pub const FLUSH_UNIT: usize = 4 << 10;
impl Wal {
    #[must_use]
    pub fn flush_unit(&self) -> usize {
        FLUSH_UNIT.min(self.ring_memory())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Precommitted {
    pub txn: u64,
    pub upto: u64,
    pub gsn: u64,
    pub remote: bool,
    pub deps: Vec<(u32, u64)>,
    pub waits: Vec<(u16, u64)>,
}
impl Wal {
    pub fn precommit(&self, txn: u64, named: &[(u16, u64, u64)]) -> Result<Precommitted, Error> {
        self.precommit_waiting(txn, named, Vec::new())
    }
    pub fn precommit_waiting(
        &self,
        txn: u64,
        named: &[(u16, u64, u64)],
        waits: Vec<(u16, u64)>,
    ) -> Result<Precommitted, Error> {
        let deps = self.outstanding()?;
        let remote = !deps.is_empty();
        let slot = self.mine()?;
        let worker = current().ok_or(Error::NoWorkerBound)?;
        let mut remote_byte = crate::wal::worker::LentRecord::take();
        let remote_byte = &mut remote_byte.0;
        payload::commit_payload(remote, named, remote_byte)?;
        let len = u32::try_from(remote_byte.len()).map_err(|_| Error::Corrupt)?;
        let header =
            Header { kind: Kind::Commit, worker, gsn: 0, page: 0, txn, len, prev: 0, lap: 0 };
        let mut queue = slot.queue_mut()?;
        let (gsn, upto) = self.append_bounded(0, header, remote_byte)?;
        let entry = Precommitted { txn, upto, gsn, remote, deps, waits };
        queue.push_back(entry.clone());
        Ok(entry)
    }
    pub fn aborted(&self, txn: u64) -> Result<u64, Error> {
        let slot = self.mine()?;
        let worker = current().ok_or(Error::NoWorkerBound)?;
        if self.live_from(usize::from(worker)).is_none() {
            return Ok(slot.ring_ref()?.written());
        }
        let header =
            Header { kind: Kind::Abort, worker, gsn: 0, page: 0, txn, len: 0, prev: 0, lap: 0 };
        self.append_bounded(0, header, &[]).map(|(_, written)| written)
    }
    pub fn releasable(&self, entry: &Precommitted) -> Result<bool, Error> {
        let worker = current().ok_or(Error::NoWorkerBound)?;
        let slot = self.mine()?;
        if slot.durable() < entry.upto {
            return Ok(false);
        }
        let _ = worker;
        for &(writer, gsn) in &entry.deps {
            if self.slots_ref().get(writer as usize).is_none() {
                return Err(Error::Corrupt);
            }
            if self.flushed().of(writer as usize) < gsn {
                return Ok(false);
            }
        }
        Ok(true)
    }
    #[must_use]
    pub fn min_gsn(&self) -> u64 {
        self.slots_ref().iter().map(|s| s.clock.now()).min().unwrap_or(0)
    }
    #[must_use]
    pub fn max_gsn(&self) -> u64 {
        self.slots_ref().iter().map(|s| s.clock.now()).max().unwrap_or(0)
    }
    pub fn barrier_transaction(&self) -> Result<bool, Error> {
        let slot = self.mine()?;
        let highest = self.max_gsn();
        if slot.clock.now() >= highest {
            return Ok(false);
        }
        slot.clock.observe(highest);
        let worker = current().ok_or(Error::NoWorkerBound)?;
        if slot.durable() >= slot.ring_ref()?.written() {
            let _ = self.flushed().publish(usize::from(worker), highest);
        }
        Ok(true)
    }
}
pub type Queue = VecDeque<Precommitted>;
