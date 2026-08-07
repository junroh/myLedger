mod memory;

pub use memory::{MemoryIdem, MemoryIdemConfig};

use ledger_base::{parts_are_sound, SizedPart, TxId};

/// What the idem component charges per unit — one part, and it is the largest single structure a
/// deployment will size.
///
/// **The count it multiplies is the one input nothing here can supply.** The window is an hour, so the
/// count is the busiest hour's transactions rather than a rate or a day's volume, and the rotating
/// generations that would enforce that hour are not built: the map only grows today. A sizing model
/// has to take the hour as an input and say so.
pub const SIZING: &[SizedPart] = &[SizedPart::table::<TxId, u64>(
    "idem keys",
    "a transaction id already seen, so a retry is answered rather than applied twice",
)];

const _: () = assert!(
    parts_are_sound(SIZING),
    "two parts share a name, or one is free"
);
