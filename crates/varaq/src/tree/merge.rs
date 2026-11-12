use super::Tree;
use crate::PageId;
use crate::bufmgr::parent::{Located, locate};
use crate::child::ChildRef;
use crate::node::Node;
impl Tree {
    pub(super) fn no_writeback_in_flight(&self, frame: u32) -> bool {
        self.pool.slot(frame).is_some_and(|s| s.frame.writebacks_in_flight() == 0)
    }
    pub(super) fn retire(&self, frame: u32, logged: Option<u8>) {
        let Some(slot) = self.pool.slot(frame) else { return };
        let page = slot.frame.page_id();
        let gsn = slot.frame.gsn();
        let mut owed = false;
        if let (Some(kind), Some(page), true) = (logged, page, self.wal.is_some())
            && slot.frame.is_dirty()
        {
            self.pool.owe_retired(frame, page, gsn, kind);
            owed = true;
        }
        let reclaimed = slot.frame.reclaim().is_ok();
        if let Some(page) = page {
            crate::bufmgr::Children::forget(self, page, frame);
        }
        if reclaimed {
            let _ = self.pool.release(frame);
        }
        if reclaimed
            && !owed
            && let Some(page) = page
        {
            self.give_back(page, gsn);
        }
    }
    pub(super) fn holds_named(&self, frame: u32, page: PageId) -> bool {
        self.pool
            .slot(frame)
            .is_some_and(|s| s.frame.state().is_readable() && s.frame.page_id() == Some(page))
            && matches!(self.translation.lookup(page), Ok(Some(named)) if named == frame)
    }
    fn mergeable(&self, child: ChildRef) -> Option<u32> {
        let at = self.frame_index(child)?;
        (self.pool.slot(at)?.frame.state() == crate::frame::State::Hot).then_some(at)
    }
    pub fn try_merge(&self, frame: u32) -> bool {
        if let Some(wal) = self.wal.as_ref()
            && !wal.room_for_optional_smo().unwrap_or(false)
        {
            return false;
        }
        let Located::At(parent) = locate(&self.pool, self, frame) else {
            return false;
        };
        let (Some(parent_slot), Some(child_slot)) =
            (self.pool.slot(parent.frame), self.pool.slot(frame))
        else {
            return false;
        };
        let Some(child_page) = child_slot.frame.page_id() else { return false };
        let Ok(mut parent_guard) = parent_slot.page.try_exclusive() else { return false };
        let Ok(mut child_guard) = child_slot.page.try_exclusive() else { return false };
        let Some(parent_page) = parent_slot.frame.page_id() else { return false };
        if !self.holds_named(frame, child_page) || !self.holds_named(parent.frame, parent_page) {
            return false;
        }
        let child = child_guard.get();
        if !child.is_under_full() {
            return false;
        }
        let parent_node = parent_guard.get();
        if Self::child_ref(&parent_node, parent.slot as usize).and_then(ChildRef::page_id)
            != Some(child_page)
        {
            return false;
        }
        if parent_node.is_empty() {
            return false;
        }
        let at = parent.slot as usize;
        let moved =
            self.merge_with_left(&mut parent_guard, &mut child_guard, at, frame, parent.frame)
                || self.merge_with_right(
                    &mut parent_guard,
                    &mut child_guard,
                    at,
                    frame,
                    parent.frame,
                );
        drop(child_guard);
        drop(parent_guard);
        if moved {
            self.merges.add(1);
            let _ = self.try_merge(parent.frame);
        }
        moved
    }
    fn latch_predecessor(
        &self,
        parent: &Node,
        freed_at: usize,
    ) -> Option<(u32, crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>)> {
        debug_assert!(freed_at > 0, "a parent's first child is never freed");
        let pred_ref = Self::child_ref(parent, freed_at - 1)?;
        let pred_frame = self.mergeable(pred_ref)?;
        let pred_slot = self.pool.slot(pred_frame)?;
        let pred_guard = pred_slot.page.try_exclusive().ok()?;
        if !pred_ref.page_id().is_some_and(|page| self.holds_named(pred_frame, page)) {
            return None;
        }
        if !pred_guard.get().is_leaf() {
            return None;
        }
        Some((pred_frame, pred_guard))
    }
    fn mark_merge(
        &self,
        pages: [(u32, &[u8]); 3],
        pred: Option<(
            u32,
            &mut crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>,
            ChildRef,
        )>,
    ) {
        let Some((pred_frame, pred_guard, survivor)) = pred else {
            self.mark_smo(&pages, 0);
            return;
        };
        pred_guard.update(|n: &mut Node| n.set_right(survivor.raw()));
        let image = Self::image_of(pred_guard);
        let [a, b, c] = pages;
        self.mark_smo(&[a, b, c, (pred_frame, &image)], 0);
    }
    fn merge_with_left(
        &self,
        parent_guard: &mut crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>,
        child_guard: &mut crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>,
        at: usize,
        frame: u32,
        parent_frame: u32,
    ) -> bool {
        if at == 0 {
            return false;
        }
        if at == 1 && child_guard.get().is_leaf() {
            return false;
        }
        let parent_node = parent_guard.get();
        let Some(left_ref) = Self::child_ref(&parent_node, at - 1) else { return false };
        let Some(left_frame) = self.mergeable(left_ref) else { return false };
        let Some(left_slot) = self.pool.slot(left_frame) else { return false };
        let Ok(mut left_guard) = left_slot.page.try_exclusive() else { return false };
        if !left_ref.page_id().is_some_and(|page| self.holds_named(left_frame, page)) {
            return false;
        }
        if !self.no_writeback_in_flight(left_frame) {
            return false;
        }
        let left = left_guard.get();
        let mut merged = child_guard.get();
        let separator = Self::separator_at(&parent_node, at - 1);
        if !left.merge_into(&mut merged, separator.as_deref()) {
            return false;
        }
        let mut pred = None;
        if merged.is_leaf() {
            let Some(survivor) = Self::child_ref(&parent_node, at) else { return false };
            let Some((pred_frame, pred_guard)) = self.latch_predecessor(&parent_node, at - 1)
            else {
                return false;
            };
            pred = Some((pred_frame, pred_guard, survivor));
        }
        child_guard.set(merged);
        let removed = parent_guard.update(|n: &mut Node| n.remove_slot(at - 1));
        if !removed {
            return false;
        }
        let left_kind = left_guard.get().kind();
        let absorbed = Self::absorbed_image(&mut left_guard, left_kind);
        self.mark_merge(
            [
                (frame, &Self::image_of(child_guard)),
                (parent_frame, &Self::image_of(parent_guard)),
                (left_frame, &absorbed),
            ],
            pred.as_mut().map(|(f, g, s)| (*f, g, *s)),
        );
        self.pool.note_move(left_frame, frame);
        self.retire(left_frame, Some(left_kind));
        drop(left_guard);
        drop(pred);
        true
    }
    fn absorbed_image(
        guard: &mut crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>,
        kind: u8,
    ) -> Vec<u8> {
        guard.set(Node::empty(kind, &[], &[]));
        Self::image_of(guard)
    }
    fn merge_with_right(
        &self,
        parent_guard: &mut crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>,
        child_guard: &mut crate::latch::ExclusiveGuard<'_, Node, { super::NODE_WORDS }>,
        at: usize,
        frame: u32,
        parent_frame: u32,
    ) -> bool {
        if at == 0 && child_guard.get().is_leaf() {
            return false;
        }
        let parent_node = parent_guard.get();
        if at >= parent_node.len() {
            return false;
        }
        let Some(right_ref) = Self::child_ref(&parent_node, at + 1) else { return false };
        let Some(right_frame) = self.mergeable(right_ref) else { return false };
        let Some(right_slot) = self.pool.slot(right_frame) else { return false };
        let Ok(mut right_guard) = right_slot.page.try_exclusive() else { return false };
        if !right_ref.page_id().is_some_and(|page| self.holds_named(right_frame, page)) {
            return false;
        }
        if !self.no_writeback_in_flight(frame) {
            return false;
        }
        let child = child_guard.get();
        let mut merged = right_guard.get();
        let separator = Self::separator_at(&parent_node, at);
        if !child.merge_into(&mut merged, separator.as_deref()) {
            return false;
        }
        let mut pred = None;
        if merged.is_leaf() {
            let Some((pred_frame, pred_guard)) = self.latch_predecessor(&parent_node, at) else {
                return false;
            };
            pred = Some((pred_frame, pred_guard, right_ref));
        }
        right_guard.set(merged);
        let removed = parent_guard.update(|n: &mut Node| n.remove_slot(at));
        if !removed {
            return false;
        }
        let kind = child_guard.get().kind();
        let absorbed = Self::absorbed_image(child_guard, kind);
        self.mark_merge(
            [
                (right_frame, &Self::image_of(&right_guard)),
                (parent_frame, &Self::image_of(parent_guard)),
                (frame, &absorbed),
            ],
            pred.as_mut().map(|(f, g, s)| (*f, g, *s)),
        );
        self.pool.note_move(frame, right_frame);
        self.retire(frame, Some(kind));
        drop(right_guard);
        drop(pred);
        true
    }
    pub(crate) fn child_ref(node: &Node, at: usize) -> Option<ChildRef> {
        if at > node.len() {
            return None;
        }
        if at == node.len() {
            let raw = node.upper();
            return (raw != 0).then(|| ChildRef::from_raw(raw));
        }
        let raw: [u8; 8] = node.payload_at(at).try_into().ok()?;
        Some(ChildRef::from_raw(u64::from_le_bytes(raw)))
    }
    fn separator_at(node: &Node, at: usize) -> Option<Vec<u8>> {
        if at >= node.len() {
            return None;
        }
        let mut key = Vec::with_capacity(node.prefix().len() + node.key_at(at).len());
        key.extend_from_slice(node.prefix());
        key.extend_from_slice(node.key_at(at));
        Some(key)
    }
}
