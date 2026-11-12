use super::insert::Expect;
use super::{Descent, Leaf, Tree};
use crate::Error;
use crate::node::Node;
use crate::node::reader::NodeRef;
impl Tree {
    pub fn remove(&self, key: &[u8]) -> Result<bool, Error> {
        self.modifying(|| self.remove_inner(key, Expect::Anything))
    }
    pub fn compare_and_remove(&self, key: &[u8], expected: &[u8]) -> Result<bool, Error> {
        self.modifying(|| self.remove_inner(key, Expect::Exactly(Some(expected))))
    }
    fn remove_inner(&self, key: &[u8], expect: Expect<'_>) -> Result<bool, Error> {
        self.retrying(|| self.try_remove(key, expect))
    }
    fn try_remove(&self, key: &[u8], expect: Expect<'_>) -> Descent<bool> {
        let Leaf { child, frame, latch, guard, .. } = self.descend(key)?;
        let separated = {
            let node = NodeRef::new(&guard);
            let (at, hit) = node.lower_bound(key)?;
            hit && node.slot(at)?.4 & crate::node::flags::OUT_OF_LINE != 0
        };
        if separated {
            if !matches!(expect, Expect::Anything) {
                guard.recheck()?;
                return Ok(false);
            }
            guard.recheck()?;
            let Some((under_full, child)) = self.try_remove_value(key)? else {
                return Err(super::Stop::Retry);
            };
            if under_full && let Some(frame) = self.frame_index(child) {
                let _ = self.try_merge(frame);
            }
            return Ok(true);
        }
        let mut exclusive = guard.upgrade().inspect_err(|_| Self::refused_by(latch))?;
        let (removed, under_full) = exclusive.update(|node: &mut Node| {
            let removed = expect.holds(node, key) && node.remove(key);
            (removed, removed && node.is_under_full())
        });
        if removed {
            let logged = self.logged_op(frame, &exclusive, &super::Op::remove(key));
            self.mark_part(&super::Part::logged(frame, &logged, &|| Self::image_of(&exclusive)));
        }
        drop(exclusive);
        if under_full && let Some(frame) = self.frame_index(child) {
            let _ = self.try_merge(frame);
        }
        Ok(removed)
    }
}
