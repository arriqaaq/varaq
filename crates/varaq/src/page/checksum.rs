use crate::{Error, PageId};
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}
pub const CHECKSUM_OFFSET: usize = 0;
pub const COVERED_FROM: usize = 4;
#[must_use]
pub fn crc32c_append(crc: u32, bytes: &[u8]) -> u32 {
    crc32c::crc32c_append(crc, bytes)
}
#[must_use]
pub fn of_page(page: &[u8], pid: PageId) -> u32 {
    assert!(page.len() > COVERED_FROM, "a page is longer than its checksum field");
    crc32c_append(crc32c(&pid.get().to_le_bytes()), &page[COVERED_FROM..])
}
pub fn stamp(page: &mut [u8], pid: PageId) {
    let sum = of_page(page, pid);
    page[CHECKSUM_OFFSET..COVERED_FROM].copy_from_slice(&sum.to_le_bytes());
}
#[must_use]
pub fn stored(page: &[u8]) -> u32 {
    assert!(page.len() >= COVERED_FROM, "a page is longer than its checksum field");
    u32::from_le_bytes([page[0], page[1], page[2], page[3]])
}
pub fn verify(page: &[u8], pid: PageId) -> Result<(), Error> {
    let stored = stored(page);
    let computed = of_page(page, pid);
    if stored == computed {
        Ok(())
    } else if page.iter().all(|&b| b == 0) {
        Err(Error::PageUnwritten { pid })
    } else {
        Err(Error::PageCorrupt { pid, stored, computed })
    }
}
