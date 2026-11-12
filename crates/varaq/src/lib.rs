#![doc = include_str!("../README.md")]
pub mod bufmgr {
    pub mod cooling;
    pub mod fault;
    pub mod freelist;
    pub mod inflight;
    pub mod parent;
    pub mod pool;
    pub mod provider;
    pub mod translate;
    pub use cooling::{CoolingStage, Verdict};
    pub use fault::fault;
    pub use freelist::FreeList;
    pub use inflight::{Claim, InflightTable, Ticket};
    pub use parent::{Children, Located, ParentRef};
    pub use pool::{PageLatch, Pool, Slot};
    pub use translate::{Translation, preferred_frame};
}
pub mod bell;
pub mod catalog;
pub mod child;
pub mod counted;
pub mod format;
pub mod frame;
pub mod latch;
pub mod layout;
pub mod linebuf;
pub mod node;
pub mod page;
pub mod recovery;
pub mod stats;
pub mod store;
pub mod sync;
pub mod task;
pub mod tree;
pub mod txn;
pub mod wal {
    pub mod barrier;
    pub mod collecting;
    pub mod driver;
    pub mod flushed;
    pub mod gsn;
    pub mod record;
    pub mod ring;
    pub mod scan;
    pub mod worker;
}
pub use bufmgr::{PageLatch, Pool, Slot};
pub use child::ChildRef;
pub use counted::{Cost, Counting};
pub use frame::{BufferFrame, ParentHint, Sample};
pub use latch::{AnyBits, ExclusiveGuard, HybridLatch, OptimisticGuard};
pub use node::structure::Split;
pub use node::{Node, PAGE_SIZE, PageKind};
pub use page::PageId;
pub use store::{MemoryStore, PageStore};
pub use task::{Scheduler, Ticket};
pub use tree::{DEFAULT_CAPACITY, Tree};
pub use txn::tuple::HEADER_BYTES as TUPLE_HEADER_BYTES;
pub use txn::{Snapshots, Timestamp, WorkerId};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceOp {
    Read,
    Write,
}
impl core::fmt::Display for DeviceOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("page id zero is reserved for the catalog and is not a data page")]
    ReservedPageId,
    #[error("the tree holds {capacity} nodes and has no room for another")]
    Full { capacity: usize },
    #[error("the entry does not fit in an empty node")]
    EntryTooLarge,
    #[error("a commit names {named} dependencies; its record can name 65535")]
    TooManyDependencies { named: usize },
    #[error("an operation retried {attempts} times without making progress")]
    Livelocked { attempts: u32 },
    #[error("a handle named a node this tree does not hold")]
    Corrupt,
    #[error("this thread modified a logged tree without being bound to a worker")]
    NoWorkerBound,
    #[error("this store could not be recovered and has not been opened")]
    RecoveryFailed,
    #[error("this store was written by {recorded} workers and opened by {requested}")]
    WorkerCountMismatch { recorded: usize, requested: usize },
    #[error("{requested} workers asked for; at most {most} can be named")]
    TooManyWorkers { requested: usize, most: usize },
    #[error("the platform refused a thread; the store goes on without it")]
    ThreadRefused,
    #[error("a catalog needs a log device: a reopen would have no log to replay")]
    CatalogWithoutLog,
    #[error("a log ring of {bytes} bytes a worker is larger than this build can address")]
    RingTooLarge { bytes: u64 },
    #[error("a log record needing {needed} bytes does not fit: {free} free of {capacity}")]
    LogFull { capacity: usize, needed: usize, free: usize },
    #[error(
        "the log refused a record for a change already applied; the store is no longer durable"
    )]
    LogPoisoned,
    #[error("the key was written by a transaction this one cannot see")]
    Conflict,
    #[error("worker {worker} already has a transaction in flight")]
    WorkerBusy { worker: usize },
    #[error("the commit's outcome is unknown and the store has stopped")]
    InDoubt,
    #[error("worker {worker} does not hold the snapshot this read was asked at")]
    SnapshotNotHeld { worker: usize },
    #[error("a lock was poisoned by a panic while it was held")]
    Poisoned,
    #[error("a pool of {frames} frames cannot be divided into {partitions} partitions")]
    PoolTooSmall { frames: usize, partitions: usize },
    #[error("frame {index} is not a slot in this pool")]
    NoSuchFrame { index: u32 },
    #[error("frame {index} still holds a page ({state:?}) and cannot be freed")]
    FrameNotFree { index: u32, state: crate::frame::State },
    #[error("page {page} could not be faulted in: {why:?}")]
    FaultFailed { page: PageId, why: crate::bufmgr::fault::Why },
    #[error(
        "the device refused to {op}{}",
        .page.map_or_else(String::new, |p|format!(" page {p}"))
    )]
    Device { page: Option<PageId>, op: DeviceOp },
    #[error("page {page} was never allocated in this store")]
    NoSuchPage { page: PageId },
    #[error("the store holds {limit} pages and will hold no more")]
    StoreFull { limit: usize },
    #[error("page {pid} failed its checksum: stored {stored:#010x}, computed {computed:#010x}")]
    PageCorrupt { pid: PageId, stored: u32, computed: u32 },
    #[error("page {pid} was allocated and never written")]
    PageUnwritten { pid: PageId },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Conflict,
    WorkerBusy,
    InDoubt,
    StoreFull,
    TooLarge,
    Busy,
    Device,
    Corrupt,
    Stopped,
    Config,
    Internal,
}
impl Class {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Conflict => "Conflict",
            Self::WorkerBusy => "WorkerBusy",
            Self::InDoubt => "InDoubt",
            Self::StoreFull => "StoreFull",
            Self::TooLarge => "TooLarge",
            Self::Busy => "Busy",
            Self::Device => "Device",
            Self::Corrupt => "Corrupt",
            Self::Stopped => "Stopped",
            Self::Config => "Config",
            Self::Internal => "Internal",
        }
    }
}
impl Error {
    #[must_use]
    pub const fn class(&self) -> Class {
        match self {
            Self::Conflict => Class::Conflict,
            Self::WorkerBusy { .. } => Class::WorkerBusy,
            Self::InDoubt => Class::InDoubt,
            Self::Full { .. } | Self::StoreFull { .. } => Class::StoreFull,
            Self::LogFull { capacity, needed, .. } => {
                if *needed > *capacity {
                    Class::TooLarge
                } else {
                    Class::StoreFull
                }
            }
            Self::EntryTooLarge | Self::TooManyDependencies { .. } => Class::TooLarge,
            Self::Livelocked { .. } => Class::Busy,
            Self::Device { .. } | Self::FaultFailed { .. } => Class::Device,
            Self::ReservedPageId
            | Self::Corrupt
            | Self::RecoveryFailed
            | Self::NoSuchPage { .. }
            | Self::PageCorrupt { .. }
            | Self::PageUnwritten { .. } => Class::Corrupt,
            Self::LogPoisoned | Self::Poisoned => Class::Stopped,
            Self::WorkerCountMismatch { .. }
            | Self::TooManyWorkers { .. }
            | Self::CatalogWithoutLog
            | Self::RingTooLarge { .. }
            | Self::PoolTooSmall { .. }
            | Self::ThreadRefused => Class::Config,
            Self::NoWorkerBound
            | Self::SnapshotNotHeld { .. }
            | Self::NoSuchFrame { .. }
            | Self::FrameNotFree { .. } => Class::Internal,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unwind;
impl core::fmt::Display for Unwind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("optimistic read observed a concurrent write; retry")
    }
}
pub type Optimistic<T> = Result<T, Unwind>;
pub mod calc;
pub mod exec;
pub mod io;
