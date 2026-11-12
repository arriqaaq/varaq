#[must_use]
#[inline]
pub fn chunk_end(next: usize, entries: usize, chunk: usize) -> usize {
    if chunk < entries - next { next + chunk } else { entries }
}
#[must_use]
#[inline]
pub fn next_chunk(chunk: usize) -> usize {
    if chunk <= usize::MAX / 2 { chunk * 2 } else { usize::MAX }
}
#[must_use]
#[inline]
fn predecessor_of(records: &[(u64, u64)], gsn: u64) -> Option<u64> {
    let mut i: usize = 0;
    while i < records.len() {
        if records[i].0 == gsn {
            return Some(records[i].1);
        }
        i += 1;
    }
    None
}
#[must_use]
pub fn chain_start(records: &[(u64, u64)], prev: u64) -> Option<u64> {
    let mut at = prev;
    loop {
        match predecessor_of(records, at) {
            None => return Some(at),
            Some(before) => {
                if before >= at {
                    return None;
                }
                at = before;
            }
        }
    }
}
#[must_use]
#[inline]
fn holds(images: &[(u64, u64)], gsn: u64, prev: u64) -> bool {
    let mut k: usize = 0;
    while k < images.len() {
        if images[k].0 == gsn && images[k].1 == prev {
            return true;
        }
        k += 1;
    }
    false
}
#[must_use]
pub fn earliest_image(records: &[(u64, u64)], images: &[(u64, u64)], prev: u64) -> u64 {
    let mut at = prev;
    let mut earliest = prev;
    loop {
        match predecessor_of(records, at) {
            None => return earliest,
            Some(before) => {
                if before >= at {
                    return earliest;
                }
                if holds(images, at, before) {
                    earliest = before;
                }
                at = before;
            }
        }
    }
}
#[must_use]
#[allow(clippy::manual_map)]
pub fn torn_start(records: &[(u64, u64)], images: &[(u64, u64)], floor: u64) -> Option<u64> {
    let mut best: Option<(u64, u64)> = None;
    let mut k: usize = 0;
    while k < images.len() {
        let gsn = images[k].0;
        let prev = images[k].1;
        let ok = match chain_start(records, prev) {
            Some(s) => s == 0 || s <= floor,
            None => false,
        };
        if ok {
            let newer = match best {
                None => true,
                Some(b) => gsn > b.0,
            };
            if newer {
                best = Some((gsn, prev));
            }
        }
        k += 1;
    }
    match best {
        None => None,
        Some(b) => Some(earliest_image(records, images, b.1)),
    }
}
#[must_use]
#[inline]
#[allow(clippy::implicit_saturating_sub)]
pub fn room(written: u64, keep: u64, capacity: u64, unit: u64) -> u64 {
    let held = written - (keep - keep % unit);
    if held <= capacity { capacity - held } else { 0 }
}
pub enum Placement {
    Fits { at: u64, written: u64 },
    DeviceFull { needed: u64, free: u64 },
    MemoryFull { needed: u64, free: u64 },
}
#[must_use]
#[inline]
pub fn place(
    written: u64,
    keep: u64,
    flushed: u64,
    device: u64,
    memory: u64,
    unit: u64,
    len: u64,
) -> Placement {
    let offset = written % device;
    let to_end = device - offset;
    let wraps = len > to_end;
    let needed = if wraps { to_end + len } else { len };
    let device_free = room(written, keep, device, unit);
    if needed > device_free {
        return Placement::DeviceFull { needed, free: device_free };
    }
    let memory_free = room(written, flushed, memory, unit);
    if needed > memory_free {
        return Placement::MemoryFull { needed, free: memory_free };
    }
    let at = if wraps { written + to_end } else { written };
    Placement::Fits { at, written: at + len }
}
#[must_use]
#[inline]
pub fn dirty(gsn: u64, on_disk: u64) -> bool {
    gsn > on_disk
}
#[must_use]
#[inline]
pub fn move_counted(gsn: u64, on_disk: u64) -> bool {
    dirty(gsn, on_disk) || gsn == 0
}
#[must_use]
#[inline]
pub fn next_multiple(n: u64, run: u64) -> Option<u64> {
    let rem = n % run;
    if rem == 0 {
        Some(n)
    } else if n - rem <= u64::MAX - run {
        Some(n - rem + run)
    } else {
        None
    }
}
#[must_use]
#[inline]
pub fn growth_target(
    grown: u64,
    end: u64,
    run: u64,
    fraction: u64,
    page: bool,
) -> Option<(u64, u64)> {
    let up = next_multiple(end, run)?;
    let least = if up > grown { up } else { grown };
    let ahead = if page { next_multiple(grown / fraction, run)? } else { 0 };
    let reach = if grown <= u64::MAX - ahead { grown + ahead } else { u64::MAX };
    let to = if least > reach { least } else { reach };
    Some((least, to))
}
#[must_use]
#[inline]
pub fn increment_range(next: usize, partitions: usize, increments: usize) -> (usize, usize, usize) {
    let start = if next < partitions { next } else { partitions };
    let rem = partitions % increments;
    let step = if rem == 0 { partitions / increments } else { partitions / increments + 1 };
    let end = if step <= partitions - start { start + step } else { partitions };
    let following = if end >= partitions { 0 } else { end };
    (start, end, following)
}
#[must_use]
#[inline]
pub fn collect_bound(oldest: Option<u64>, floor: u64) -> u64 {
    match oldest {
        Some(at) => {
            if at < floor {
                at
            } else {
                floor
            }
        }
        None => floor,
    }
}
#[must_use]
pub fn balanced_split_point(sizes: &[u32]) -> usize {
    let n = sizes.len();
    let mut total: u64 = 0;
    let mut i: usize = 0;
    while i < n {
        total += u64::from(sizes[i]);
        i += 1;
    }
    let mut running: u64 = 0;
    let mut i: usize = 0;
    while i < n {
        running += u64::from(sizes[i]);
        if running * 2 >= total {
            let at = i + 1;
            return if at < 1 {
                1
            } else if at > n - 1 {
                n - 1
            } else {
                at
            };
        }
        i += 1;
    }
    1
}
#[must_use]
#[inline]
#[allow(clippy::fn_params_excessive_bools)]
pub fn covers_by(
    lower_empty: bool,
    lower_below_key: bool,
    upper_empty: bool,
    key_at_or_below_upper: bool,
) -> bool {
    (lower_empty || lower_below_key) && (upper_empty || key_at_or_below_upper)
}
#[inline]
pub fn lower_bound_by<E, F: Fn(usize) -> Result<core::cmp::Ordering, E>>(
    n: usize,
    cmp: F,
) -> Result<(usize, bool), E> {
    let mut lo: usize = 0;
    let mut hi: usize = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match cmp(mid) {
            Ok(core::cmp::Ordering::Less) => lo = mid + 1,
            Ok(core::cmp::Ordering::Greater) => hi = mid,
            Ok(core::cmp::Ordering::Equal) => return Ok((mid, true)),
            Err(e) => return Err(e),
        }
    }
    Ok((lo, false))
}
