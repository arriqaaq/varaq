use std::collections::{BTreeMap, BTreeSet};
#[derive(Default, Debug)]
pub struct Space {
    by_first: BTreeMap<u64, (u64, u64)>,
    by_size: BTreeSet<(u64, u64)>,
}
impl Space {
    pub fn give(&mut self, first: u64, pages: u64, stamp: u64) {
        if pages == 0 {
            return;
        }
        debug_assert!(!self.overlaps(first, pages), "pages {first}+{pages} were freed twice");
        let (mut first, mut pages, mut stamp) = (first, pages, stamp);
        if let Some((&before, &(n, s))) = self.by_first.range(..first).next_back()
            && before + n == first
        {
            self.remove(before, n);
            (first, pages, stamp) = (before, pages + n, stamp.max(s));
        }
        if let Some(&(n, s)) = self.by_first.get(&(first + pages)) {
            self.remove(first + pages, n);
            (pages, stamp) = (pages + n, stamp.max(s));
        }
        self.by_first.insert(first, (pages, stamp));
        self.by_size.insert((pages, first));
    }
    pub fn take(&mut self, pages: u64) -> Option<(u64, u64)> {
        let &(n, first) = self.by_size.range((pages, 0)..).next()?;
        let (_, stamp) = self.remove(first, n);
        if n > pages {
            self.by_first.insert(first + pages, (n - pages, stamp));
            self.by_size.insert((n - pages, first + pages));
        }
        Some((first, stamp))
    }
    pub fn take_ending_at(&mut self, last: u64) -> Option<(u64, u64, u64)> {
        let (&first, &(n, _)) = self.by_first.range(..=last).next_back()?;
        if first + n - 1 != last {
            return None;
        }
        let (n, stamp) = self.remove(first, n);
        Some((first, n, stamp))
    }
    #[must_use]
    pub fn pages(&self) -> u64 {
        self.by_first.values().map(|&(n, _)| n).sum()
    }
    pub fn extents(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.by_first.iter().map(|(&first, &(n, s))| (first, n, s))
    }
    #[must_use]
    pub fn overlaps(&self, first: u64, pages: u64) -> bool {
        let end = first + pages;
        self.by_first.range(..end).next_back().is_some_and(|(&f, &(n, _))| f + n > first)
    }
    fn remove(&mut self, first: u64, pages: u64) -> (u64, u64) {
        self.by_size.remove(&(pages, first));
        self.by_first.remove(&first).unwrap_or((0, 0))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    pub worker: usize,
    pub end: u64,
    pub gsn: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    pub first: u64,
    pub pages: u64,
    pub release: u64,
    pub cause: At,
    pub forgot: bool,
}
#[derive(Default, Debug)]
pub struct Ledger {
    pub free: Space,
    pub pending: Vec<Pending>,
    pub owned: BTreeMap<u64, (u64, At)>,
    pub in_flight: BTreeMap<u64, (u64, u64, Option<At>)>,
}
impl Ledger {
    pub fn release(&mut self, settled: impl Fn(u64) -> bool) {
        let free = &mut self.free;
        self.pending.retain(|p| {
            if p.forgot && settled(p.release) {
                free.give(p.first, p.pages, p.release);
                false
            } else {
                true
            }
        });
        self.in_flight.retain(|_, (_, _, at)| at.is_none_or(|a| !settled(a.gsn)));
    }
    pub fn listing(
        &mut self,
        begins: &[u64],
        settled: impl Fn(u64) -> bool,
    ) -> Vec<(u64, u64, u64)> {
        let before = |at: &At| begins.get(at.worker).is_some_and(|&b| at.end <= b);
        let applies = |at: &At| before(at) || settled(at.gsn);
        let mut out: Vec<(u64, u64, u64)> = self.free.extents().collect();
        out.extend(
            self.pending
                .iter()
                .filter(|p| applies(&p.cause))
                .map(|p| (p.first, p.pages, p.cause.gsn)),
        );
        out.extend(
            self.owned
                .iter()
                .filter(|(_, (_, at))| applies(at))
                .map(|(&f, &(n, at))| (f, n, at.gsn)),
        );
        out.extend(
            self.in_flight
                .iter()
                .filter(|(_, (_, _, at))| at.as_ref().is_none_or(|a| !before(a)))
                .map(|(&f, &(n, stamp, _))| (f, n, stamp)),
        );
        self.in_flight.retain(|_, (_, _, at)| at.as_ref().is_none_or(|a| !before(a)));
        out
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Became {
    Free,
    Named,
}
#[must_use]
pub fn rebuild(
    listed: &[(u64, u64, u64)],
    events: &[(u64, u64, u64, Became)],
    restored: &[(u64, u64)],
    reclaim: core::ops::Range<u64>,
    named: &BTreeSet<u64>,
) -> Vec<(u64, u64)> {
    let mut paints: Vec<(u64, u64, u64, Became)> =
        listed.iter().map(|&(f, n, s)| (s, f, n, Became::Free)).collect();
    paints.extend(events.iter().map(|&(f, n, g, b)| (g, f, n, b)));
    paints.sort_by_key(|&(g, f, _, b)| (g, b == Became::Named, f));
    let mut map: BTreeMap<u64, (u64, Became)> = BTreeMap::new();
    for (_, first, pages, became) in paints {
        paint(&mut map, first, first + pages, became);
    }
    for &(first, pages) in restored {
        paint(&mut map, first, first + pages, Became::Named);
    }
    let mut unpainted = Vec::new();
    let mut at = reclaim.start;
    for (&s, &(e, _)) in map.range(..reclaim.end) {
        if e <= at {
            continue;
        }
        if s > at {
            unpainted.push((at, s.min(reclaim.end)));
        }
        at = at.max(e);
    }
    if at < reclaim.end {
        unpainted.push((at, reclaim.end));
    }
    for (s, e) in unpainted {
        let mut from = s;
        for &n in named.range(s..e) {
            if n > from {
                paint(&mut map, from, n, Became::Free);
            }
            from = n + 1;
        }
        if from < e {
            paint(&mut map, from, e, Became::Free);
        }
    }
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (&s, &(e, became)) in &map {
        if became != Became::Free {
            continue;
        }
        match out.last_mut() {
            Some((f, n)) if *f + *n == s => *n += e - s,
            _ => out.push((s, e - s)),
        }
    }
    out
}
fn paint(map: &mut BTreeMap<u64, (u64, Became)>, first: u64, end: u64, became: Became) {
    if first >= end {
        return;
    }
    if let Some((&s, &(e, b))) = map.range(..first).next_back()
        && e > first
    {
        map.insert(s, (first, b));
        if e > end {
            map.insert(end, (e, b));
        }
    }
    let inside: Vec<u64> = map.range(first..end).map(|(&s, _)| s).collect();
    for s in inside {
        if let Some((e, b)) = map.remove(&s)
            && e > end
        {
            map.insert(end, (e, b));
        }
    }
    map.insert(first, (end, became));
}
