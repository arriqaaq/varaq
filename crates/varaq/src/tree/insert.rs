use super::{Descent, Fresh, Leaf, RETRY_BOUND, Stop, Tree};
use crate::Error;
use crate::bufmgr::pool::PageGuard;
use crate::child::ChildRef;
use crate::node::structure::Split;
use crate::node::{Node, PageKind};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attempt {
    Inserted,
    Full,
    Mismatch,
    Separated,
    NeedsPage,
}
#[derive(Debug, Clone, Copy)]
pub(super) enum Expect<'a> {
    Anything,
    Exactly(Option<&'a [u8]>),
    Head(&'a [u8]),
    Blob(crate::node::blob::Ref, &'a [u8]),
}
impl Expect<'_> {
    pub(super) fn matches_entry<const W: usize>(
        self,
        entry: Option<(&crate::node::reader::NodeRef<'_, '_, W>, usize)>,
    ) -> crate::Optimistic<bool> {
        Ok(match (self, entry) {
            (Self::Anything, _) => true,
            (Self::Exactly(want), None) => want.is_none(),
            (Self::Exactly(None), Some(_)) | (Self::Head(_), None) | (Self::Blob(..), _) => false,
            (Self::Exactly(Some(want)), Some((node, at))) => node.payload_equals(at, want)?,
            (Self::Head(head), Some((node, at))) => node.payload_starts_with(at, head)?,
        })
    }
    pub(super) fn matches_split<const W: usize>(
        self,
        local: &[u8],
        page: &crate::node::reader::NodeRef<'_, '_, W>,
        at: usize,
    ) -> crate::Optimistic<bool> {
        Ok(match self {
            Self::Anything => true,
            Self::Exactly(None) | Self::Blob(..) => false,
            Self::Exactly(Some(want)) => {
                want.len() >= local.len()
                    && &want[..local.len()] == local
                    && page.payload_equals(at, &want[local.len()..])?
            }
            Self::Head(head) => {
                let here = head.len().min(local.len());
                head[..here] == local[..here]
                    && (head.len() <= local.len()
                        || page.payload_starts_with(at, &head[local.len()..])?)
            }
        })
    }
    pub(super) fn holds(self, node: &Node, key: &[u8]) -> bool {
        match self {
            Self::Anything => true,
            Self::Exactly(want) => {
                let (at, hit) = node.lower_bound(key);
                let current = if hit { Some(node.payload_at(at)) } else { None };
                current == want
            }
            Self::Head(head) => {
                let (at, hit) = node.lower_bound(key);
                hit && node.payload_at(at).starts_with(head)
            }
            Self::Blob(..) => false,
        }
    }
}
impl Tree {
    pub fn insert(&self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.modifying(|| self.insert_inner(key, value))
    }
    fn insert_inner(&self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        self.insert_expecting(key, value, Expect::Anything, None).map(|_| ())
    }
    pub fn compare_and_insert(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        value: &[u8],
    ) -> Result<bool, Error> {
        self.modifying(|| self.insert_expecting(key, value, Expect::Exactly(expected), None))
    }
    pub fn compare_and_insert_logged(
        &self,
        key: &[u8],
        expected: Option<&[u8]>,
        value: &[u8],
        undo: super::TxnUndo<'_>,
    ) -> Result<bool, Error> {
        self.modifying(|| self.insert_expecting(key, value, Expect::Exactly(expected), Some(undo)))
    }
    pub fn compare_head_and_insert(
        &self,
        key: &[u8],
        head: &[u8],
        value: &[u8],
    ) -> Result<bool, Error> {
        self.modifying(|| self.insert_expecting(key, value, Expect::Head(head), None))
    }
    pub(super) fn insert_expecting(
        &self,
        key: &[u8],
        value: &[u8],
        expect: Expect<'_>,
        undo: Option<super::TxnUndo<'_>>,
    ) -> Result<bool, Error> {
        let staged = self.stage_blob(key, value)?;
        let done = self.insert_staged(key, value, expect, undo, staged);
        if let Some(staged) = staged
            && !matches!(done, Ok(true))
        {
            self.unstage(staged);
        }
        done
    }
    pub(super) fn insert_staged(
        &self,
        key: &[u8],
        value: &[u8],
        expect: Expect<'_>,
        undo: Option<super::TxnUndo<'_>>,
        staged: Option<super::blob::Staged>,
    ) -> Result<bool, Error> {
        match self.retrying(|| self.try_insert(key, value, expect, undo, staged))? {
            Attempt::Inserted => return Ok(true),
            Attempt::Mismatch => return Ok(false),
            Attempt::Full | Attempt::Separated | Attempt::NeedsPage => {}
        }
        self.insert_splitting(key, value, expect, undo, staged)
    }
    fn try_insert(
        &self,
        key: &[u8],
        value: &[u8],
        expect: Expect<'_>,
        undo: Option<super::TxnUndo<'_>>,
        staged: Option<super::blob::Staged>,
    ) -> Descent<Attempt> {
        if staged.is_some() || crate::node::separates(self.separate_above, key.len(), value.len()) {
            return self.try_insert_value(key, value, expect, undo, staged);
        }
        let Leaf { frame, latch, guard, .. } = self.descend_from(key, undo.and_then(|u| u.from))?;
        if self.separate_above != usize::MAX {
            let node = crate::node::reader::NodeRef::new(&guard);
            let (at, hit) = node.lower_bound(key)?;
            if hit && node.slot(at)?.4 & crate::node::flags::OUT_OF_LINE != 0 {
                guard.recheck()?;
                return self.try_insert_value(key, value, expect, undo, None);
            }
        }
        let mut exclusive = guard.upgrade().inspect_err(|_| Self::refused_by(latch))?;
        let held = Self::overwrite_in_place(&mut exclusive, key, value, expect)?;
        let outcome = held.map_or_else(
            || {
                exclusive.update_ranges(|node: &mut Node| {
                    let (at, hit) = node.lower_bound(key);
                    if hit && node.flags_at(at) & crate::node::flags::OUT_OF_LINE != 0 {
                        return ((Attempt::Separated, false), [(0, 0); 2]);
                    }
                    if !expect.holds(node, key) {
                        return ((Attempt::Mismatch, false), [(0, 0); 2]);
                    }
                    let before = node.extent();
                    let (inserted, compacted) = node.insert_reporting(key, value, 0);
                    let ranges =
                        if inserted { node.dirty_ranges(before, compacted) } else { [(0, 0); 2] };
                    let added = node.len() > before.0;
                    ((if inserted { Attempt::Inserted } else { Attempt::Full }, added), ranges)
                })
            },
            |held| (held, false),
        );
        let (outcome, added) = outcome;
        if outcome == Attempt::Inserted {
            let op = super::Op::put(added, key, value, 0);
            let logged = self.logged_op(frame, &exclusive, &op);
            let image = || Self::image_of(&exclusive);
            let part = super::Part::logged(frame, &logged, &image);
            match undo {
                Some(undo) => self.mark_change(&[part], undo),
                None => self.mark_part(&part),
            }
        }
        if outcome == Attempt::Separated {
            drop(exclusive);
            return self.try_insert_value(key, value, expect, undo, None);
        }
        Ok(outcome)
    }
    fn overwrite_in_place<const W: usize>(
        exclusive: &mut crate::latch::ExclusiveGuard<'_, Node, W>,
        key: &[u8],
        value: &[u8],
        expect: Expect<'_>,
    ) -> crate::Optimistic<Option<Attempt>> {
        let node = crate::node::reader::NodeRef::held(exclusive);
        let (at, hit) = node.lower_bound(key)?;
        if !hit {
            return Ok(None);
        }
        let (offset, key_len, payload_len, _, flags) = node.slot(at)?;
        if !Node::replaces_in_place((flags, payload_len), 0, value.len()) {
            return Ok(None);
        }
        if !expect.matches_entry(Some((&node, at)))? {
            return Ok(Some(Attempt::Mismatch));
        }
        let written = exclusive.write_at(offset + key_len, value);
        debug_assert!(written, "a payload inside its page was refused");
        Ok(Some(Attempt::Inserted))
    }
    fn abandon_split(&self, frames: &[u32]) {
        for &frame in frames {
            self.retire(frame, None);
        }
    }
    fn insert_splitting(
        &self,
        key: &[u8],
        value: &[u8],
        expect: Expect<'_>,
        undo: Option<super::TxnUndo<'_>>,
        staged: Option<super::blob::Staged>,
    ) -> Result<bool, Error> {
        let mut refusals = super::Refusals::default();
        loop {
            let kept = crate::node::separated::local_len(self.keep_local, value.len());
            let in_leaf: &[u8] = if let Some(staged) = staged {
                &[0_u8; crate::node::blob::PAYLOAD_MOST]
                    [..crate::node::blob::REFERENCE_BYTES + staged.local]
            } else if crate::node::separates(self.separate_above, key.len(), value.len()) {
                &[0_u8; crate::node::separated::PAYLOAD_MOST]
                    [..crate::node::separated::REFERENCE_BYTES + kept]
            } else {
                value
            };
            let (spent, refused) = match self.split_toward(key, in_leaf) {
                Ok(()) => match self.try_insert(key, value, expect, undo, staged) {
                    Ok(Attempt::Inserted) => return Ok(true),
                    Ok(Attempt::Mismatch) => return Ok(false),
                    Ok(Attempt::Full | Attempt::Separated | Attempt::NeedsPage) => {
                        (refusals.other(), false)
                    }
                    Err(Stop::Retry) => (refusals.retry(), true),
                    Err(Stop::Misrouted) => (refusals.other(), true),
                    Err(Stop::Refuse(e)) => return Err(e),
                },
                Err(Stop::Retry) => {
                    self.smo_restarts.add(1);
                    (refusals.retry(), true)
                }
                Err(Stop::Misrouted) => {
                    self.smo_restarts.add(1);
                    (refusals.other(), true)
                }
                Err(Stop::Refuse(e)) => return Err(e),
            };
            if spent {
                return Err(Error::Livelocked { attempts: RETRY_BOUND });
            }
            if refused {
                self.between_attempts(&refusals)?;
            }
        }
    }
    fn split_toward(&self, key: &[u8], value: &[u8]) -> Descent<()> {
        let _in_smo = self.enter_smo();
        let mut path: Vec<super::Level<'_>> = Vec::with_capacity(8);
        self.descend_trailed(key, &mut path)?;
        let last = path.len().checked_sub(1).ok_or(Stop::Retry)?;
        let mut node = path[last].guard.read()?;
        if node.has_room_for(key, value) {
            return Ok(());
        }
        let mut at = last;
        loop {
            let mut lhs = Node::new(PageKind::Leaf, b"", b"");
            let mut rhs = Node::new(PageKind::Leaf, b"", b"");
            let split =
                node.split_for(Some(key), &mut lhs, &mut rhs).ok_or(Error::EntryTooLarge)?;
            if at == 0 {
                return self.split_root(&path[0], key);
            }
            let parent = path[at - 1].guard.read()?;
            if parent.has_room_for(&split.separator, &[0_u8; 8]) {
                return self.split_below(&path[at - 1], &path[at], key);
            }
            node = parent;
            at -= 1;
        }
    }
    fn split_root(&self, root: &super::Level<'_>, key: &[u8]) -> Descent<()> {
        let root_ref = root.child;
        let root_node = root.guard.read()?;
        let mut lhs = Node::new(PageKind::Leaf, b"", b"");
        let mut rhs = Node::new(PageKind::Leaf, b"", b"");
        let split =
            root_node.split_for(Some(key), &mut lhs, &mut rhs).ok_or(Error::EntryTooLarge)?;
        let is_leaf = root_node.is_leaf();
        let (allocated, kept, allocated_image) = if is_leaf {
            rhs.set_right(root_node.right());
            let right = self.allocate(&rhs)?;
            lhs.set_right(right.child().raw());
            (right, lhs, Self::image_of_node(&rhs))
        } else {
            (self.allocate(&lhs)?, rhs, Self::image_of_node(&lhs))
        };
        let allocated_page = allocated;
        let allocated = allocated_page.child();
        let mut parent =
            Node::new(PageKind::Inner, root_node.lower_fence(), root_node.upper_fence());
        let (below, above) = if is_leaf { (root_ref, allocated) } else { (allocated, root_ref) };
        if !parent.insert(&split.separator, &below.raw().to_le_bytes()) {
            self.abandon_split(&[allocated_page.frame()]);
            return Err(Stop::Refuse(Error::EntryTooLarge));
        }
        parent.set_upper(above.raw());
        let new_root_page = match self.allocate(&parent) {
            Ok(page) => page,
            Err(e) => {
                self.abandon_split(&[allocated_page.frame()]);
                return Err(Stop::Refuse(e));
            }
        };
        let new_root = new_root_page.child();
        let Ok(mut root_pointer) = self.root.exclusive() else {
            Self::refused_by(&self.root);
            self.abandon_split(&[allocated_page.frame(), new_root_page.frame()]);
            return Err(Stop::Retry);
        };
        if root_pointer.get() != root_ref {
            drop(root_pointer);
            self.abandon_split(&[allocated_page.frame(), new_root_page.frame()]);
            return Err(Stop::Retry);
        }
        let Ok(mut root_holder) = root.guard.clone().upgrade() else {
            Self::refused_by(root.resolved.latch);
            drop(root_pointer);
            self.abandon_split(&[allocated_page.frame(), new_root_page.frame()]);
            return Err(Stop::Retry);
        };
        if self.confirm(&root.resolved).is_err() {
            drop(root_holder);
            drop(root_pointer);
            self.abandon_split(&[allocated_page.frame(), new_root_page.frame()]);
            return Err(Stop::Retry);
        }
        root_holder.set(kept);
        let kept_image = Self::image_of(&root_holder);
        let new_root_image = Self::image_of_node(&parent);
        self.mark_smo(
            &[
                (root.resolved.frame, &kept_image),
                (allocated_page.frame(), &allocated_image),
                (new_root_page.frame(), &new_root_image),
            ],
            new_root.page_id().map_or(0, crate::PageId::get),
        );
        drop(allocated_page);
        drop(new_root_page);
        drop(root_holder);
        root_pointer.set(new_root);
        drop(root_pointer);
        self.record_parent_hint(new_root, below, 0);
        self.record_parent_hint(new_root, above, 1);
        Ok(())
    }
    fn split_inner(
        &self,
        parent_guard: &mut PageGuard<'_>,
        child_guard: &mut PageGuard<'_>,
        _child: ChildRef,
        split: &Split,
        lhs: &Node,
        rhs: &Node,
    ) -> Descent<(Fresh<'_>, usize)> {
        let left = self.allocate(lhs)?;
        child_guard.overwrite(rhs);
        if !parent_guard
            .update(|n: &mut Node| n.insert(&split.separator, &left.child().raw().to_le_bytes()))
        {
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let at = parent_guard.get().lower_bound(&split.separator).0;
        Ok((left, at))
    }
    fn split_leaf(
        &self,
        parent_guard: &mut PageGuard<'_>,
        child_guard: &mut PageGuard<'_>,
        child: ChildRef,
        key: &[u8],
        split: &Split,
        lhs: &mut Node,
        rhs: &mut Node,
    ) -> Descent<(Fresh<'_>, usize)> {
        let named_at = parent_guard.get().lower_bound(key).0;
        let was_upper = named_at >= parent_guard.get().len();
        rhs.set_right(child_guard.get().right());
        let right = self.allocate(rhs)?;
        lhs.set_right(right.child().raw());
        child_guard.overwrite(lhs);
        if !parent_guard
            .update(|n: &mut Node| n.insert(&split.separator, &child.raw().to_le_bytes()))
        {
            return Err(Stop::Refuse(Error::Corrupt));
        }
        let repointed = parent_guard.update(|n: &mut Node| {
            if was_upper {
                n.set_upper(right.child().raw());
                true
            } else {
                n.replace_payload(named_at + 1, &right.child().raw().to_le_bytes())
            }
        });
        if !repointed {
            return Err(Stop::Refuse(Error::Corrupt));
        }
        Ok((right, named_at))
    }
    fn split_below(
        &self,
        parent: &super::Level<'_>,
        child: &super::Level<'_>,
        key: &[u8],
    ) -> Descent<()> {
        let mut parent_guard = parent
            .guard
            .clone()
            .upgrade()
            .inspect_err(|_| Self::refused_by(parent.resolved.latch))?;
        self.confirm(&parent.resolved)?;
        let mut child_guard = child
            .guard
            .clone()
            .upgrade()
            .inspect_err(|_| Self::refused_by(child.resolved.latch))?;
        self.confirm(&child.resolved)?;
        let node = parent_guard.get();
        let child_node = child_guard.get();
        let mut lhs = Node::new(PageKind::Leaf, b"", b"");
        let mut rhs = Node::new(PageKind::Leaf, b"", b"");
        let split =
            child_node.split_for(Some(key), &mut lhs, &mut rhs).ok_or(Error::EntryTooLarge)?;
        if !node.has_room_for(&split.separator, &[0_u8; 8]) {
            return Err(Stop::Retry);
        }
        let (fresh, at) = if child_node.is_leaf() {
            self.split_leaf(
                &mut parent_guard,
                &mut child_guard,
                child.child,
                key,
                &split,
                &mut lhs,
                &mut rhs,
            )?
        } else {
            self.split_inner(&mut parent_guard, &mut child_guard, child.child, &split, &lhs, &rhs)?
        };
        let allocated_image = if child_node.is_leaf() {
            Self::image_of_node(&rhs)
        } else {
            Self::image_of_node(&lhs)
        };
        let new_page = fresh.child();
        let (child_image, parent_image) =
            (Self::image_of(&child_guard), Self::image_of(&parent_guard));
        self.mark_smo(
            &[
                (child.resolved.frame, &child_image),
                (fresh.frame(), &allocated_image),
                (parent.resolved.frame, &parent_image),
            ],
            0,
        );
        drop(fresh);
        drop(child_guard);
        drop(parent_guard);
        self.record_parent_hint(parent.child, new_page, at);
        Ok(())
    }
}
