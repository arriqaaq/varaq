use super::{DATA_BYTES, Node, SLOT_BYTES};
impl Node {
    pub fn remove_slot(&mut self, i: usize) -> bool {
        let n = self.len();
        if i >= n {
            return false;
        }
        let s = self.slot(i);
        self.space_used -= s.key_len + s.payload_len;
        for j in i..n - 1 {
            let next = self.slot(j + 1);
            self.set_slot(j, next);
        }
        self.count -= 1;
        true
    }
    pub fn remove(&mut self, key: &[u8]) -> bool {
        match self.lower_bound(key) {
            (i, true) => self.remove_slot(i),
            (_, false) => false,
        }
    }
    #[must_use]
    pub fn free_space_after_compaction(&self) -> usize {
        DATA_BYTES.saturating_sub(self.len() * SLOT_BYTES).saturating_sub(self.space_used as usize)
    }
    #[must_use]
    pub fn is_under_full(&self) -> bool {
        self.free_space_after_compaction() >= Self::UNDER_FULL
    }
    pub const UNDER_FULL: usize = DATA_BYTES * 3 / 5;
}
