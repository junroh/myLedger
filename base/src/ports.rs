pub mod account;
pub mod idempotency;
pub mod pending;
pub mod raft;

pub use account::{AccountFlags, AccountPort, AccountRecord, LedgerTotals};
pub use idempotency::{IdemAsk, IdemReply, IdemRequest, IdemVerdict, IdempotencyPort};
pub use pending::{
    HoldData, HoldView, OverlayState, PendingCommand, PendingEffect, PendingFence, PendingLookup,
    PendingNotice, PendingOverlay, PendingPort, PendingReply,
};
pub use raft::{ApplyIndex, RaftCommit, RaftOutcome, RaftPort, RaftProposal};

/// Attached to a request so a reply needs no lookup. Only the sequencer reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct Correlation(pub u32);

impl Correlation {
    pub const fn raw(self) -> u32 {
        self.0
    }
}
