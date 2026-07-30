use crate::ids::{AccountId, Amount, BudgetGroup, Seq, TxId};
use crate::ports::Correlation;

/// Hold record as stored by the pending engine. It provides data and judges nothing.
#[derive(Debug, Clone, Copy)]
pub struct HoldData {
    pub debit_account: AccountId,
    pub credit_account: AccountId,
    pub amount: Amount,
    pub remaining: Amount,
    pub ledger: u32,
    /// Absent unless the hold shares a budget with others that must be resolved with it.
    pub budget: BudgetGroup,
    /// The group as the store sees it, so a resolution can be checked for coverage. Zero when the
    /// hold belongs to no group.
    pub budget_members: u32,
    pub budget_remaining: Amount,
}

/// Committed sequencer decisions the pending engine writes down as-is.
#[derive(Debug, Clone, Copy)]
pub enum PendingEffect {
    Create {
        tx_id: TxId,
        debit_account: AccountId,
        credit_account: AccountId,
        amount: Amount,
        ledger: u32,
        budget: BudgetGroup,
    },
    Reduce {
        pending_ref: TxId,
        remaining: Amount,
    },
    Remove {
        pending_ref: TxId,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct PendingLookup {
    pub correlation: Correlation,
    pub tx_id: TxId,
    pub lane: AccountId,
    pub seq: Seq,
    pub pending_ref: TxId,
}

/// Ordering token for a request that needs no hold data, so it cannot overtake one that does.
#[derive(Debug, Clone, Copy)]
pub struct PendingFence {
    pub correlation: Correlation,
    pub lane: AccountId,
    pub seq: Seq,
}

/// `pending_ref` is absent on a fence reply: there is no hold to seed, only order to keep.
#[derive(Debug, Clone, Copy)]
pub struct PendingReply {
    pub correlation: Correlation,
    pub lane: AccountId,
    pub seq: Seq,
    pub pending_ref: TxId,
    pub found: Option<HoldData>,
}

/// One channel, because order matters: a lookup issued after a hold was removed must not see the
/// store as it was before, and a fence must not pass the lookup it sits behind.
#[derive(Debug, Clone, Copy)]
pub enum PendingCommand {
    Lookup(PendingLookup),
    Fence(PendingFence),
    Apply(PendingEffect),
}

/// Whether the engine's overlay can answer about a hold without a round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    /// Nothing known yet: the sequencer must ask for it.
    Absent,
    /// A lookup is already in flight, so a later request only needs to be ordered behind it.
    LookupSent,
    /// Answerable inline.
    Ready,
    /// The engine looked and the hold is not there. Asking again would get the same answer, because
    /// a write always reaches the store before a later lookup.
    Missing,
}

/// What the judge needs about a hold, with uncommitted reservations already deducted.
#[derive(Debug, Clone, Copy)]
pub struct HoldView {
    pub debit_account: AccountId,
    pub credit_account: AccountId,
    pub ledger: u32,
    pub budget: BudgetGroup,
    pub budget_members: u32,
    pub budget_remaining: Amount,
    /// Committed remainder minus what proposed-but-uncommitted resolutions already took.
    pub remaining: Amount,
    /// Whether an in-flight resolution has already consumed the hold entirely.
    pub resolved: bool,
}

/// Two ways in. The overlay answers inline, because the judge cannot continue without knowing how
/// much of a hold is left; it carries a copy of what the store last confirmed plus the uncommitted
/// reservations against it, which exist nowhere else. Everything else is a queue: the sequencer
/// sends and moves on, a full queue is backpressure, and replies come back in each lane's seq order.
pub trait PendingPort {
    /// Takes `&mut self` because the engine may update its own overlay from what it is told — a
    /// hold it has just been asked to create needs no lookup to be known.
    fn send(&mut self, command: PendingCommand) -> Result<(), PendingCommand>;
    fn poll(&self) -> Option<PendingReply>;

    fn overlay_state(&self, hold: TxId) -> OverlayState;
    /// Records that a lookup is on the way.
    fn begin_lookup(&mut self, hold: TxId);
    /// Takes the answer, `None` included: an answer of "not there" is as good as any other.
    fn admit_lookup(&mut self, hold: TxId, found: Option<HoldData>);
    fn view(&self, hold: TxId) -> Option<HoldView>;

    /// A request that will read this hold is in flight, so the engine must keep it whatever its
    /// eviction policy says. Balanced by `unpin` when that request is answered — otherwise a hold
    /// evicted between the answer and the judgment would reject a resolution of a hold that exists.
    fn pin(&mut self, hold: TxId);
    fn unpin(&mut self, hold: TxId);

    /// This much of the hold is spoken for; `resolves` when nothing will be left.
    fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool);
    /// The batch committed, so this reservation is no longer speculative. What the hold has left
    /// follows from the `Apply` the engine is sent, not from this call.
    fn release_reservation(&mut self, hold: TxId, amount: Amount);
    /// Give the reservation back after a failed commit.
    fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool);

    /// Eviction by the engine's own policy, driven from the reactor's tick.
    fn maintain(&mut self) -> usize;
    fn overlay_len(&self) -> usize;
}
