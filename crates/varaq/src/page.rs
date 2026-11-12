pub mod checksum;
use crate::Error;
pub use checksum::{crc32c, stamp, verify};
const CATALOG: u64 = 0;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(u64);
impl PageId {
    pub const CATALOG: Self = Self(CATALOG);
    pub const fn new(raw: u64) -> Result<Self, Error> {
        if raw == CATALOG {
            return Err(Error::ReservedPageId);
        }
        Ok(Self(raw))
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
impl core::fmt::Display for PageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}
