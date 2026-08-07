mod block;
mod cache;
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

pub use block::{
    DurableStore, LogTraffic, MemoryStore, ObjectId, OpenBacking, RecordAddr, StoreModel,
    VolumeStats, BLOCK_BYTES, DEFAULT_FLUSH_BLOCKS, DEFAULT_RESIDENT_BLOCKS, RECORDS_PER_BLOCK,
    RECORD_BYTES, SEGMENTS,
};
/// The seam under the engine, in a filesystem's vocabulary: a segment is a file, brought into being by its
/// first block and removed whole, and a block is bytes at an offset that follows from its address alone. So
/// what answers for them can be memory today and a file or a network volume later without the engine above
/// changing, and without anything having to be restored to know where a block sits. Design notes §16.
pub use cache::Cached;
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

/// What the pending engine charges per unit. The longest list, because this is the component whose
/// size a rate and a retention actually decide.
///
/// **Three units and they are not interchangeable.** A slot is linear — the index allocates what it was
/// told and nothing more. A bucket is a staircase — a hash table rounds up to a power of two, so one
/// percent more entries can double it. A block is 4KB whether or not the records in it are live.
///
/// The three block counts are the engine's own three words for one set of records, and they answer
/// three questions: `writeback buffer` is what a restart would have to replay, `resident blocks` is what
/// memory keeps so a resolution need not read a device, and `stored blocks` is what the store holds —
/// the only one that becomes a disk figure once the store is a directory.
///
/// `pending record` is the odd one: it is the eighty bytes a hold costs inside a block, so it is a unit
/// rather than a structure and no footprint reports it. It belongs here anyway — a sizing answer that
/// reported memory and left the record size to a second source is how the two drift apart.
pub const SIZING: &[ledger_base::SizedPart] = &[
    ledger_base::SizedPart::new("pending index", ledger_base::Unit::Slot, index::SLOT_BYTES),
    ledger_base::SizedPart::table::<ledger_base::BudgetGroup, engine::BudgetState>(
        "pending budget groups",
    ),
    ledger_base::SizedPart::new(
        "pending overlay",
        ledger_base::Unit::Bucket,
        overlay::ENTRY_BUCKET_BYTES,
    ),
    ledger_base::SizedPart::new(
        "pending writeback buffer",
        ledger_base::Unit::Block,
        block::BLOCK_BYTES,
    ),
    ledger_base::SizedPart::new(
        "pending resident blocks",
        ledger_base::Unit::Block,
        block::BLOCK_BYTES,
    ),
    ledger_base::SizedPart::new(
        "pending stored blocks",
        ledger_base::Unit::Block,
        block::BLOCK_BYTES,
    ),
    ledger_base::SizedPart::new(
        "volume read cache",
        ledger_base::Unit::Block,
        block::BLOCK_BYTES,
    ),
    ledger_base::SizedPart::new(
        "volume write lane",
        ledger_base::Unit::Block,
        block::BLOCK_BYTES,
    ),
    ledger_base::SizedPart::new(
        "volume read pool",
        ledger_base::Unit::Block,
        block::BLOCK_BYTES,
    ),
    ledger_base::SizedPart::new(
        "pending record",
        ledger_base::Unit::Record,
        block::RECORD_BYTES,
    ),
];

const _: () = assert!(
    ledger_base::parts_are_sound(SIZING),
    "two parts share a name, or one is free"
);
