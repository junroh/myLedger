mod memory;
mod overlay;

pub use memory::{MemoryPending, MemoryPendingConfig};
/// Exported for a simulation that drives the engine itself: the overlay is where holds, their
/// uncommitted reservations and the pins live, and a simulation that reimplemented it would be
/// exercising something else.
pub use overlay::HoldOverlay;
