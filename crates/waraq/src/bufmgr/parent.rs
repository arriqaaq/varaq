use crate::bufmgr::pool::Pool;
use crate::child::ChildRef;
use crate::frame::{NO_FRAME, State};
use crate::{Optimistic, PageId};
pub trait Children {
    fn child_at(&self, frame: u32, slot: u32) -> Optimistic<Option<ChildRef>>;
    fn is_root(&self, frame: u32) -> Optimistic<bool>;
    fn child_count(&self, frame: u32) -> Optimistic<u32>;
    fn resident_frame(&self, pool: &Pool, child: ChildRef) -> Optimistic<Option<u32>>;
    fn forget(&self, page: PageId, frame: u32);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentRef {
    pub frame: u32,
    pub slot: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a merge that ignores where the parent is will unlink the wrong child"]
pub enum Located {
    At(ParentRef),
    Root,
    Unwind(Why),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    NoHint,
    HintStale,
    HintMoved,
    Contended,
    NotResident,
}
pub fn locate(pool: &Pool, children: &impl Children, child: u32) -> Located {
    match children.is_root(child) {
        Ok(true) => return Located::Root,
        Ok(false) => {}
        Err(crate::Unwind) => return Located::Unwind(Why::Contended),
    }
    let Some(slot) = pool.slot(child) else {
        return Located::Unwind(Why::HintStale);
    };
    let Some((parent_frame, parent_slot, recorded)) = slot.frame.parent.get() else {
        return Located::Unwind(Why::NoHint);
    };
    if parent_frame == NO_FRAME {
        return Located::Unwind(Why::NoHint);
    }
    let Some(parent) = pool.slot(parent_frame) else {
        return Located::Unwind(Why::HintStale);
    };
    let before = parent.frame.sample();
    if before.version() != recorded {
        return Located::Unwind(Why::HintStale);
    }
    if !before.state().is_readable() {
        return Located::Unwind(match before.state() {
            State::Loading => Why::Contended,
            _ => Why::NotResident,
        });
    }
    let Ok(found) = children.child_at(parent_frame, parent_slot) else {
        return Located::Unwind(Why::Contended);
    };
    if !parent.frame.recheck(before) {
        return Located::Unwind(Why::HintStale);
    }
    match found {
        Some(reference) if points_at(pool, reference, child) => {
            Located::At(ParentRef { frame: parent_frame, slot: parent_slot })
        }
        _ => Located::Unwind(Why::HintMoved),
    }
}
fn points_at(pool: &Pool, child: ChildRef, frame: u32) -> bool {
    let Some(slot) = pool.slot(frame) else { return false };
    let named = child.page_id();
    named.is_some() && named == slot.frame.page_id()
}
