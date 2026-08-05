//! Issues order, judges consistency, proposes, applies.
//!
//! Two mechanisms are easy to confuse. `rules::linked` is atomicity within one submission and
//! lasts for one judgment. `rules::budget` is a property of holds that outlives the request that
//! created them. A budget group is resolved by a linked chain; that is the only place they meet.
//!
//! Judging adds up four things, and only the first is durable:
//!
//! - committed balances, in `AccountRecord`, owned by the account component
//! - the speculative overlay, in `LaneState`: availability promised to requests that
//!   are proposed but not committed, reducing deltas only
//! - the pending overlay, owned by the pending engine and read behind `PendingPort::view`: hold
//!   remainder already taken by resolutions that are proposed but not committed
//! - the chain scratch, in `LinkedScratch`: what a chain's own earlier legs bring
//!   in, visible only inside that chain
//!
//! Both overlays are one leader's guesses. They are taken when a request is judged and resolved
//! when its batch commits or fails, so a failed commit can only cause a false reject.

mod config;
mod log_kind;
mod metrics;
mod reactor;
mod rules;
mod state;

pub use config::{BatchPolicy, Capacity, LinkedPolicy, ReactorConfig, SafetyPolicy};
pub use log_kind::LogKind;
pub use metrics::{Metrics, StageTimes};
pub use reactor::{Backpressure, Broken, Reactor, Transport};
pub use state::lane::{LaneState, LaneTable};

use ledger_base::{layouts_are_sound, TypeLayout};

/// Declared and checked at each struct; gathered here for reporting.
pub const HOT_TYPES: &[TypeLayout] = &[state::pipeline::LAYOUT, state::lane::LAYOUT];

const _: () = assert!(
    layouts_are_sound(HOT_TYPES),
    "a watched type broke its layout contract"
);
