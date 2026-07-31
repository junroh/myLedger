mod block;
mod engine;
mod index;
mod memory;
mod orderer;
mod overlay;

pub use memory::{MemoryPending, MemoryPendingConfig, PendingCapacity, StoreModel};
/// Exported for the same reason as the overlay: a simulation that drives the engine itself needs the
/// store the engine actually keeps, and one that reimplemented it would be exercising something else.
pub use engine::{PendingEngine, Started};
/// The seam under the engine. Blocks are bytes so that what answers for them can be memory today and
/// a file or a network volume later without the engine above changing.
pub use block::{
    BlockAddr, BlockStore, LatencyBlockStore, LogTraffic, MemBlockStore, BLOCK_BYTES,
    DEFAULT_FLUSH_BLOCKS, DEFAULT_RESIDENT_BLOCKS, RECORDS_PER_BLOCK, RECORD_BYTES,
};
/// Exported for a simulation that drives the engine itself: the overlay is where holds, their
/// uncommitted reservations and the pins live, and a simulation that reimplemented it would be
/// exercising something else.
pub use overlay::HoldOverlay;
/// Contract 1 is the engine's own work, so the structure that keeps it lives here. Exported for a
/// simulation that drives the engine and wants to see what ordering cost it.
pub use orderer::{Orderer, OrderWait};
/// Exported for this crate's own bench: what the index costs as it fills is the number an analytic
/// estimate of it was guessing, and it can only be measured from outside.
pub use index::{Candidates, HoldTable, Homeless, DEFAULT_SLOTS, LOAD_TARGET, SLOT_BYTES};

/// The layout claims this crate owns, printed by `ledgerfio layout` beside everyone else's.
pub const HOT_TYPES: &[ledger_base::TypeLayout] = &[index::BUCKET_LAYOUT];

const _: () = assert!(
    ledger_base::layouts_are_sound(HOT_TYPES),
    "a watched type broke its layout contract"
);
