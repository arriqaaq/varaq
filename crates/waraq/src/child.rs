use crate::PageId;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChildRef(u64);
impl ChildRef {
    #[must_use]
    pub const fn evicted(page: PageId) -> Self {
        Self(page.get())
    }
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
    #[must_use]
    pub fn page_id(self) -> Option<PageId> {
        PageId::new(self.0).ok()
    }
}
