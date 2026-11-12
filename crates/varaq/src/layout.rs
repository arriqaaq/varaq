use crate::HybridLatch;
use crate::bufmgr::cooling::CoolingStage;
use crate::bufmgr::freelist::FreeList;
use crate::bufmgr::inflight::InflightTable;
use crate::bufmgr::pool::{PageLatch, Slot};
use crate::bufmgr::provider::{Budget, Counters, Sampler};
use crate::child::ChildRef;
use crate::frame::{BufferFrame, ParentHint, Sample};
use crate::node::{Node, PAGE_SIZE};
use crate::page::PageId;
use core::mem::{align_of, size_of};
pub const CACHE_LINE: usize = 64;
pub const NODE_IS_ONE_PAGE: () = assert!(size_of::<Node>() == PAGE_SIZE);
pub const PAGE_ID_IS_ONE_WORD: () = assert!(size_of::<PageId>() == 8);
pub const LATCH_COSTS_ONE_WORD: () =
    assert!(size_of::<HybridLatch<u64, 1>>() == size_of::<u64>() + 8);
pub const PAGE_LATCH_COSTS_ONE_WORD: () = assert!(size_of::<PageLatch>() == PAGE_SIZE + 8);
pub const CHILD_REF_IS_ONE_WORD: () = assert!(size_of::<ChildRef>() == 8);
pub const CHILD_REF_IS_WORD_ALIGNED: () = assert!(align_of::<ChildRef>() == 8);
pub const FRAME_HEADER_FITS_A_CACHE_LINE: () = assert!(size_of::<BufferFrame>() <= CACHE_LINE);
pub const PARENT_HINT_IS_TWO_WORDS: () = assert!(size_of::<ParentHint>() == 16);
pub const SAMPLE_IS_ONE_WORD: () = assert!(size_of::<Sample>() == 8);
pub const FREE_LIST_IS_A_WHOLE_CACHE_LINE: () = assert!(size_of::<FreeList>() == CACHE_LINE);
pub const FREE_LIST_IS_CACHE_ALIGNED: () = assert!(align_of::<FreeList>() == CACHE_LINE);
pub const INFLIGHT_TABLE_IS_CACHE_ALIGNED: () = assert!(align_of::<InflightTable>() == CACHE_LINE);
pub const COOLING_STAGE_IS_CACHE_ALIGNED: () = assert!(align_of::<CoolingStage>() == CACHE_LINE);
pub const SLOT_STARTS_ON_A_CACHE_LINE: () = assert!(align_of::<Slot>() >= CACHE_LINE);
pub const SLOT_IS_NOT_A_POWER_OF_TWO: () = assert!(
    size_of::<Slot>() > PAGE_SIZE && !size_of::<Slot>().is_power_of_two(),
    "a slot rounded to a power of two puts every frame header in one associativity set"
);
pub const SLOT_OVERHEAD_IS_BOUNDED: () = assert!(size_of::<Slot>() - PAGE_SIZE <= 128);
pub const SAMPLER_IS_ONE_WORD: () = assert!(size_of::<Sampler>() == 8);
pub const BUDGET_IS_SMALL: () = assert!(size_of::<Budget>() <= 24);
#[must_use]
pub fn table() -> Vec<(&'static str, usize, usize)> {
    vec![
        ("Node", size_of::<Node>(), align_of::<Node>()),
        ("HybridLatch", size_of::<HybridLatch<u64, 1>>(), align_of::<HybridLatch<u64, 1>>()),
        ("PageLatch", size_of::<PageLatch>(), align_of::<PageLatch>()),
        ("PageId", size_of::<PageId>(), align_of::<PageId>()),
        ("ChildRef", size_of::<ChildRef>(), align_of::<ChildRef>()),
        ("BufferFrame", size_of::<BufferFrame>(), align_of::<BufferFrame>()),
        ("Sample", size_of::<Sample>(), align_of::<Sample>()),
        ("ParentHint", size_of::<ParentHint>(), align_of::<ParentHint>()),
        ("FreeList", size_of::<FreeList>(), align_of::<FreeList>()),
        ("InflightTable", size_of::<InflightTable>(), align_of::<InflightTable>()),
        ("CoolingStage", size_of::<CoolingStage>(), align_of::<CoolingStage>()),
        ("Slot", size_of::<Slot>(), align_of::<Slot>()),
        ("Sampler", size_of::<Sampler>(), align_of::<Sampler>()),
        ("Budget", size_of::<Budget>(), align_of::<Budget>()),
        ("Counters", size_of::<Counters>(), align_of::<Counters>()),
    ]
}
