use crate::Error;
use crate::txn::commits::Commits;
use crate::txn::versions::Versions;
use crate::txn::{Timestamp, WorkerId, tuple};
use std::borrow::Cow;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reader {
    pub(crate) worker: WorkerId,
    pub(crate) at: Timestamp,
}
#[derive(Debug, Default)]
pub struct Dependencies {
    taken: Vec<(WorkerId, u64)>,
    settled: usize,
}
impl Dependencies {
    fn take(&mut self, writer: WorkerId, tag: u64) {
        let dep = (writer, tag);
        if !self.taken.contains(&dep) {
            self.taken.push(dep);
        }
    }
    pub(crate) fn absorb(&mut self, seen: Self) {
        if self.taken.is_empty() {
            self.taken = seen.taken;
            return;
        }
        for (writer, tag) in seen.taken {
            self.take(writer, tag);
        }
    }
    pub(crate) fn unsettled(&mut self) -> &[(WorkerId, u64)] {
        let from = std::mem::replace(&mut self.settled, self.taken.len());
        &self.taken[from..]
    }
    pub(crate) fn taken(&self) -> &[(WorkerId, u64)] {
        &self.taken
    }
    pub(crate) fn into_taken(self) -> Vec<(WorkerId, u64)> {
        self.taken
    }
}
pub(crate) fn may_see(
    reader: Reader,
    writer: WorkerId,
    tag: u64,
    commits: &Commits,
    deps: &mut Dependencies,
) -> Result<bool, Error> {
    if writer == reader.worker && tag == reader.at.get() {
        return Ok(true);
    }
    let seen = commits.visible(writer, tag, reader.at)?;
    if seen && !commits.is_acked(writer, tag)? {
        deps.take(writer, tag);
    }
    Ok(seen)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Value,
    Removed,
    Behind,
}
pub fn head_visible(
    bytes: &[u8],
    whole: bool,
    reader: Reader,
    commits: &Commits,
    deps: &mut Dependencies,
) -> Result<Seen, Error> {
    let (writer, tx_ts, removed) = match tuple::decode_head(bytes)? {
        Some(tuple::Head::VisibleForAll) => return Ok(Seen::Value),
        Some(tuple::Head::Chained { writer, tx_ts }) => (writer, tx_ts, false),
        Some(tuple::Head::Removed { writer, tx_ts }) => (writer, tx_ts, true),
        None if whole => return Err(Error::Corrupt),
        None => return Ok(Seen::Behind),
    };
    if (whole || bytes.len() > tuple::CHAINED_HEADER_BYTES)
        && !tuple::version_len_fits(removed, bytes.len())
    {
        return Err(Error::Corrupt);
    }
    if !may_see(reader, WorkerId(usize::from(writer)), tx_ts, commits, deps)? {
        return Ok(Seen::Behind);
    }
    Ok(if removed { Seen::Removed } else { Seen::Value })
}
pub fn visible<'a>(
    stored: &'a [u8],
    reader: Reader,
    (versions, runs): (&Versions, &crate::Tree),
    commits: &Commits,
    deps: &mut Dependencies,
) -> Result<Option<Cow<'a, [u8]>>, Error> {
    let head = match tuple::decode(stored)? {
        tuple::Stored::VisibleForAll(payload) => return Ok(Some(Cow::Borrowed(payload))),
        tuple::Stored::Chained(header, payload) => {
            if may_see(reader, WorkerId(usize::from(header.writer)), header.tx_ts, commits, deps)? {
                return Ok(Some(Cow::Borrowed(payload)));
            }
            header
        }
        tuple::Stored::Removed(header) => {
            if may_see(reader, WorkerId(usize::from(header.writer)), header.tx_ts, commits, deps)? {
                return Ok(None);
            }
            header
        }
    };
    let mut writer = WorkerId(usize::from(head.writer));
    let mut next = head.previous;
    let mut passed = Brent::new((writer, next));
    loop {
        if next == tuple::NO_PREVIOUS {
            return Ok(None);
        }
        let Some(bytes) = versions.get(writer, next)? else {
            return Err(Error::Corrupt);
        };
        let token = crate::node::blob::of_token(&bytes).is_some();
        let mut head = [0_u8; tuple::CHAINED_HEADER_BYTES];
        let decided: &[u8] = if token {
            let n = runs.stored_prefix(&bytes, &mut head)?;
            &head[..n]
        } else {
            &bytes
        };
        let link = match tuple::decode(decided)? {
            tuple::Stored::VisibleForAll(_) => return Err(Error::Corrupt),
            tuple::Stored::Chained(header, payload) => {
                writer = WorkerId(usize::from(header.writer));
                if may_see(reader, writer, header.tx_ts, commits, deps)? {
                    if !token {
                        return Ok(Some(Cow::Owned(payload.to_vec())));
                    }
                    let mut whole = runs.value_of_stored(&bytes)?;
                    if !matches!(
                        tuple::decode(& whole) ?, tuple::Stored::Chained(h, _) if h ==
                        header
                    ) {
                        return Err(Error::Corrupt);
                    }
                    whole.drain(..tuple::CHAINED_HEADER_BYTES);
                    return Ok(Some(Cow::Owned(whole)));
                }
                header
            }
            tuple::Stored::Removed(_) if token => return Err(Error::Corrupt),
            tuple::Stored::Removed(header) => {
                writer = WorkerId(usize::from(header.writer));
                if may_see(reader, writer, header.tx_ts, commits, deps)? {
                    return Ok(None);
                }
                header
            }
        };
        next = link.previous;
        if passed.revisits((writer, next)) {
            return Err(Error::Corrupt);
        }
    }
}
struct Brent<T> {
    mark: T,
    power: u64,
    steps: u64,
}
impl<T: PartialEq + Copy> Brent<T> {
    const fn new(first: T) -> Self {
        Self { mark: first, power: 1, steps: 0 }
    }
    fn revisits(&mut self, link: T) -> bool {
        if link == self.mark {
            return true;
        }
        self.steps += 1;
        if self.steps == self.power {
            self.mark = link;
            self.power = self.power.saturating_mul(2);
            self.steps = 0;
        }
        false
    }
}
