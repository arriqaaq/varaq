use super::{DATA_BYTES, Node, PageKind, SLOT_BYTES};
#[derive(Clone, Debug)]
pub struct Split {
    pub separator: Vec<u8>,
}
impl Node {
    pub fn compact(&mut self) -> usize {
        let before = self.free_space();
        let mut fresh = Self::empty(self.kind, self.lower_fence(), self.upper_fence());
        fresh.upper = self.upper;
        fresh.right = self.right;
        fresh.gsn = self.gsn;
        let prefix = self.prefix();
        for i in 0..self.len() {
            if !fresh.insert_parts(prefix, self.key_at(i), self.payload_at(i), self.flags_at(i)) {
                return 0;
            }
        }
        *self = fresh;
        self.free_space().saturating_sub(before)
    }
    #[must_use]
    pub fn split_into(&self, left: &mut Self, right: &mut Self) -> Option<Split> {
        self.split_for(None, left, right)
    }
    #[must_use]
    pub fn split_for(
        &self,
        key: Option<&[u8]>,
        left: &mut Self,
        right: &mut Self,
    ) -> Option<Split> {
        if self.len() < 2 || self.kind == PageKind::Value as u8 {
            return None;
        }
        let last = self.len() - 1;
        let balanced = || {
            let mut sizes = [0_u32; crate::node::DATA_BYTES / SLOT_BYTES];
            let n = self.len().min(sizes.len());
            for (i, size) in sizes.iter_mut().enumerate().take(n) {
                let bytes = self.key_at(i).len() + self.payload_at(i).len() + SLOT_BYTES;
                *size = u32::try_from(bytes).unwrap_or(u32::MAX).min(65_536);
            }
            debug_assert!(
                n >= 2 && n == self.len(),
                "a balanced split of {n} of {} entries",
                self.len()
            );
            crate::calc::balanced_split_point(&sizes[..n])
        };
        let skewed = match key.map(|k| self.lower_bound(k)) {
            Some((i, false)) if i >= self.len() => Some(last),
            Some((0, false)) => Some(1),
            _ => None,
        };
        for at in skewed.into_iter().chain(core::iter::once_with(balanced)) {
            if let Some(split) = self.split_at(at, left, right) {
                return Some(split);
            }
        }
        None
    }
    fn split_at(&self, at: usize, left: &mut Self, right: &mut Self) -> Option<Split> {
        let prefix = self.prefix();
        let mut separator = Vec::with_capacity(prefix.len() + self.key_at(at - 1).len());
        separator.extend_from_slice(prefix);
        separator.extend_from_slice(self.key_at(at - 1));
        *left = Self::empty(self.kind, self.lower_fence(), &separator);
        *right = Self::empty(self.kind, &separator, self.upper_fence());
        let boundary = if self.is_leaf() { at } else { at - 1 };
        for i in 0..boundary {
            if !left.insert_parts(prefix, self.key_at(i), self.payload_at(i), self.flags_at(i)) {
                return None;
            }
        }
        for i in at..self.len() {
            if !right.insert_parts(prefix, self.key_at(i), self.payload_at(i), self.flags_at(i)) {
                return None;
            }
        }
        if !self.is_leaf() {
            let raw: [u8; 8] = self.payload_at(at - 1).try_into().ok()?;
            left.set_upper(u64::from_le_bytes(raw));
            right.set_upper(self.upper());
        }
        Some(Split { separator })
    }
}
impl Node {
    #[must_use]
    pub fn merge_space_upper_bound(&self, right: &Self) -> usize {
        let merged = Self::shared_prefix(self.lower_fence(), right.upper_fence());
        let left_grow = (self.prefix_len as usize).saturating_sub(merged) * self.len();
        let right_grow = (right.prefix_len as usize).saturating_sub(merged) * right.len();
        self.space_used as usize
            + right.space_used as usize
            + (self.len() + right.len()) * SLOT_BYTES
            + left_grow
            + right_grow
    }
    fn shared_prefix(a: &[u8], b: &[u8]) -> usize {
        if b.is_empty() {
            return 0;
        }
        a.iter().zip(b).take_while(|(x, y)| x == y).count()
    }
    #[must_use]
    pub fn is_left_of(&self, right: &Self) -> bool {
        !self.upper_fence().is_empty() && self.upper_fence() == right.lower_fence()
    }
    #[must_use]
    pub fn merge_into(&self, right: &mut Self, separator: Option<&[u8]>) -> bool {
        if self.kind != right.kind || self.kind == PageKind::Value as u8 || !self.is_left_of(right)
        {
            return false;
        }
        if self.merge_space_upper_bound(right) > DATA_BYTES {
            return false;
        }
        let upper = right.upper();
        let mut merged = Self::empty(self.kind, self.lower_fence(), right.upper_fence());
        if !self.is_leaf() {
            let Some(sep) = separator else { return false };
            if !merged.insert(sep, &self.upper().to_le_bytes()) {
                return false;
            }
        }
        for side in [self, &*right] {
            for i in 0..side.len() {
                let (key, payload) = (side.key_at(i), side.payload_at(i));
                if !merged.insert_parts(side.prefix(), key, payload, side.flags_at(i)) {
                    return false;
                }
            }
        }
        merged.set_upper(upper);
        merged.set_right(right.right());
        *right = merged;
        true
    }
}
