//! What a stand-in for an external component needs, and the ledger does not.
//!
//! The components around the sequencer are not built yet, so they are simulated: they answer after a
//! delay, they run on their own thread, and they honour the ordering contract the sequencer only
//! checks. That machinery lives here rather than in `ledger-base`, so the contracts crate carries no
//! implementation of a contract — and no way to violate one on purpose. When a real component
//! arrives it brings its own ordering, and this crate leaves its dependency graph.

mod lane_order;
mod latency;
mod server;
mod worker;

pub use lane_order::LaneOrderer;
pub use latency::LatencyRange;
pub use server::{Server, ServerStats};
pub use worker::{IdleBackoff, WorkerThread};
