mod block;
mod engine;
mod files;
mod index;
mod memory;
mod orderer;
mod overlay;
mod snapshot;
mod snapshots;
#[cfg(test)]
mod testkit;

/// The seam under the engine, in a filesystem's vocabulary: a segment is a file, brought into being by its
/// first block and removed whole, and a block is bytes at an offset that follows from its address alone. So
/// what answers for them can be memory today and a file or a network volume later without the engine above
/// changing, and without anything having to be restored to know where a block sits. Design notes §16.
pub use block::{
    DurableStore, LogTraffic, MemoryStore, ObjectId, OpenBacking, RecordAddr, StoreModel,
    BLOCK_BYTES, DEFAULT_FLUSH_BLOCKS, DEFAULT_RESIDENT_BLOCKS, RECORDS_PER_BLOCK, RECORD_BYTES,
    SEGMENTS,
};
/// Exported for the same reason as the overlay: a simulation that drives the engine itself needs the
/// store the engine actually keeps, and one that reimplemented it would be exercising something else.
pub use engine::{NotStored, PendingEngine, Started};
/// Exported for this crate's own bench: what the index costs as it fills is the number an analytic
/// estimate of it was guessing, and it can only be measured from outside.
pub use index::{Candidates, HoldTable, Homeless, DEFAULT_SLOTS, LOAD_TARGET, SLOT_BYTES};
pub use memory::{DaySource, MemoryPending, MemoryPendingConfig, PendingCapacity, PendingStorage};
/// Contract 1 is the engine's own work, so the structure that keeps it lives here. Exported for a
/// simulation that drives the engine and wants to see what ordering cost it.
pub use orderer::{OrderWait, Orderer};
/// Exported for a simulation that drives the engine itself: the overlay is where holds, their
/// uncommitted reservations and the pins live, and a simulation that reimplemented it would be
/// exercising something else.
pub use overlay::HoldOverlay;
/// What the engine writes down so a log can be truncated. The same bytes a follower receives instead of
/// entries and a restart reads instead of replaying from nothing — see design notes §15.
pub use snapshot::{NotASnapshot, SnapshotReader, SnapshotWriter, RECORD as SNAPSHOT_RECORD};
/// Where those bytes go, and what paces them there. Through the store, which is the one path to a disk,
/// on a volume of its own or the blocks' depending on what the deployment declared — design notes §19
/// and §20.
pub use snapshots::{
    SnapshotPolicy, SnapshotStats, Snapshots,
    DEFAULT_BYTES_PER_ROUND as DEFAULT_SNAPSHOT_BYTES_PER_ROUND,
    DEFAULT_SHADOW_BUDGET as DEFAULT_SNAPSHOT_SHADOW_BUDGET,
};

/// The layout claims this crate owns, printed by `ledgerfio layout` beside everyone else's.
pub const HOT_TYPES: &[ledger_base::TypeLayout] = &[index::BUCKET_LAYOUT, block::BLOCK_LAYOUT];

const _: () = assert!(
    ledger_base::layouts_are_sound(HOT_TYPES),
    "a watched type broke its layout contract"
);
