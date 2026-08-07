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
pub use reactor::{Backpressure, Broken, PauseCause, PressureView, Reactor, Transport};
pub use state::lane::{LaneState, LaneTable};

use ledger_base::{layouts_are_sound, parts_are_sound, SizedPart, TypeLayout, Unit};

/// Declared and checked at each struct; gathered here for reporting.
pub const HOT_TYPES: &[TypeLayout] = &[state::pipeline::LAYOUT, state::lane::LAYOUT];

const _: () = assert!(
    layouts_are_sound(HOT_TYPES),
    "a watched type broke its layout contract"
);

/// What the reactor charges per unit, for sizing a machine it has never run on. Names match what
/// `Reactor::footprint` reports, so a prediction and a run line up by name.
///
/// Two of these are the working set and the rest are in flight, and the split is the whole point of
/// listing them apart: lanes follow the accounts a deployment has, while slots, acks and batches
/// follow what the components' latencies leave outstanding.
pub const SIZING: &[SizedPart] = &[
    SizedPart::new("work slots", Unit::Slot, state::pipeline::SLOT_BYTES),
    SizedPart::new(
        "deferred dispatches",
        Unit::Slot,
        state::batcher::SLOT_ID_BYTES,
    ),
    SizedPart::new("lane state", Unit::Account, state::lane::LANE_BYTES),
    SizedPart::new(
        "open batch effects",
        Unit::Effect,
        state::batcher::EFFECT_BYTES,
    ),
    SizedPart::new(
        "batches awaiting consensus",
        Unit::Batch,
        state::batcher::BATCH_BYTES,
    ),
    // Counted per effect of batch room, not per buffer: the two pools hold one effect and one slot id
    // for each, and how much room a buffer has is the batch ceiling rather than anything fixed. So the
    // count a model multiplies is spare buffers times that ceiling.
    SizedPart::new(
        "spare batch buffers",
        Unit::Effect,
        state::batcher::EFFECT_BYTES + state::batcher::SLOT_ID_BYTES,
    ),
    SizedPart::new("ack backlog", Unit::Entry, state::outbox::ACK_BYTES),
    SizedPart::new(
        "queued pending writes",
        Unit::Effect,
        state::pending::PENDING_EFFECT_BYTES,
    ),
];

const _: () = assert!(
    parts_are_sound(SIZING),
    "two parts share a name, or one is free"
);
