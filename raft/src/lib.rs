mod echo;

pub use echo::{EchoRaft, EchoRaftConfig};

use ledger_base::{parts_are_sound, SizedPart, Unit};

/// What consensus charges per unit. **Neither part has a bound today** and the list says so by what it
/// leaves to the caller: the log has no compaction, so its count is every effect committed since the
/// last snapshot rather than a steady state, and a model that multiplies it by a day is describing a
/// node nobody would run.
pub const SIZING: &[SizedPart] = &[
    SizedPart::new("kept log", Unit::Effect, echo::LOG_EFFECT_BYTES),
    SizedPart::new("proposals in flight", Unit::Batch, echo::PROPOSAL_BYTES),
];

const _: () = assert!(
    parts_are_sound(SIZING),
    "two parts share a name, or one is free"
);
