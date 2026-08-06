use crate::ids::{AccountId, Amount, BudgetGroup, Seq, TxId};
use crate::ports::Correlation;
use crate::transfer::Transfer;

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
    /// Append-only means a changed remainder is a new record, so this carries the whole record rather
    /// than a delta: everything but the hold's original size is on the effect already, and that one field
    /// comes from the record this request was answered with. Zero means there was none — a resolution
    /// judged inside the chain that created the hold — and the engine reads the old version instead.
    Reduce {
        pending_ref: TxId,
        debit_account: AccountId,
        credit_account: AccountId,
        /// The hold's original size, or zero when the sequencer could not supply it.
        amount: Amount,
        remaining: Amount,
        /// What this resolution took, for the group's total. Carried because the engine would otherwise
        /// have to read the old remainder back to subtract it.
        consumed: Amount,
        ledger: u32,
        budget: BudgetGroup,
    },
    Remove {
        pending_ref: TxId,
        /// The group this hold belonged to, and what it takes out of it. Carried so the store needs no
        /// record to keep a group's total: the decision already knows both, and reading one back would
        /// be an IO on the path that applies committed effects in order.
        budget: BudgetGroup,
        released: Amount,
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
    /// Committed decisions the engine had applied when it answered. The sequencer knows how many it had
    /// sent when it asked, and fewer than that means the engine answered from state older than its own
    /// queue — a data check where the lane's order used to be the check. It fits in the padding this
    /// struct already had, so it costs nothing per reply.
    pub applied: u64,
}

/// One channel, because order matters: a lookup issued after a hold was removed must not see the
/// store as it was before, and a fence must not pass the lookup it sits behind.
#[derive(Debug, Clone, Copy)]
pub enum PendingCommand {
    Lookup(PendingLookup),
    Fence(PendingFence),
    Apply(PendingEffect),
}

/// What the engine says without being asked.
///
/// Everything else on this port answers a command the sequencer sent, and carries the
/// [`Correlation`] of the request that sent it. A notice answers nothing and names no request, so
/// it cannot travel as a reply: a sentinel correlation would be one field meaning two things. It
/// travels its own way for a second reason too — a notice is on no request's latency path, and a
/// notice the sequencer is slow to take must not delay a reply that is.
#[derive(Debug, Clone, Copy)]
pub enum PendingNotice {
    /// A hold consensus committed that the engine could not store: its index was sized for a
    /// declared maximum and that maximum has been passed. The log says the hold exists and no
    /// resolution of it can ever be answered, so there is nothing to retry and nothing to fix here
    /// — which is why this is news the sequencer has to act on rather than a number to report.
    HoldNotStored { hold: TxId },
    /// A hold whose record has reached the end of its retention. The engine proposes releasing what is
    /// left of it, and the sequencer judges that like any other resolution — it has to, because a
    /// resolution the client submitted may be in flight for the same hold, and only the judge can see
    /// both.
    ///
    /// It carries a whole [`Transfer`] rather than a hold id because a resolution *is* a transfer, and
    /// building one needs the record — the two accounts, the ledger — which the engine has and the
    /// sequencer does not. The engine reads that record as part of the sweep; it needs it either way.
    HoldExpired { void: Transfer },
}

/// What the sequencer has decided about a hold and not handed over yet. It exists nowhere else: the
/// store only learns a decision when its batch commits. Everything a hold *is* — its accounts, its
/// ledger, its group — comes from the record instead, because that is where it lives.
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayState {
    /// The remainder the sequencer last told the engine this hold has. `None` when it has told it
    /// nothing, which makes the record's own remainder the newest there is.
    pub remaining: Option<Amount>,
    /// How much of that remainder proposed-but-uncommitted resolutions have taken.
    pub taken: Amount,
    /// Whether one of them consumed the hold entirely.
    pub resolved: bool,
}

/// What the judge needs about a hold: the record the engine answered with, with the sequencer's own
/// uncommitted decisions already folded in.
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

impl HoldView {
    /// The one place the two halves meet. The record cannot be older than the overlay: a remainder only
    /// ever decreases, so the smaller of two observations of it is the newer one — and the sequencer's is
    /// taken the moment it decides, while the engine's answer can have been in flight across that.
    pub fn compose(record: &HoldData, overlay: OverlayState) -> Self {
        let committed = overlay
            .remaining
            .unwrap_or(record.remaining)
            .min(record.remaining);
        Self {
            debit_account: record.debit_account,
            credit_account: record.credit_account,
            ledger: record.ledger,
            budget: record.budget,
            budget_members: record.budget_members,
            budget_remaining: record.budget_remaining,
            remaining: committed - overlay.taken,
            resolved: overlay.resolved,
        }
    }
}

/// The **inline** contract, and the reason there are two: the judge cannot continue without knowing
/// what a hold has left, and that part is the sequencer's own — reservations taken at propose and
/// released at apply, which no store has been told about yet. So this half runs on the caller's own
/// thread, answers immediately, and cannot refuse. It is bounded by the requests in flight, and it
/// holds no copy of a record: the record belongs to the engine, and the reply carries it to the
/// request that asked.
pub trait PendingOverlay {
    /// The engine looked and the hold is not there. Asking again would get the same answer, because a
    /// write always reaches the store before a later lookup — so this is the one thing a resolution can
    /// be told without a round trip.
    fn hold_is_missing(&self, hold: TxId) -> bool;
    /// A lookup is on the way, so this hold needs somewhere for its pins to go.
    fn begin_lookup(&mut self, hold: TxId);
    /// Takes the answer's remainder, `None` meaning the hold is not there. The record itself goes to
    /// the request that asked for it.
    fn admit_lookup(&mut self, hold: TxId, remaining: Option<Amount>);
    /// A hold the engine has just been told to create has all of itself left, and any earlier answer of
    /// "not there" is now wrong.
    fn created(&mut self, hold: TxId, amount: Amount);
    fn overlay(&self, hold: TxId) -> OverlayState;

    /// A request that will read this hold is in flight, so its decisions must be kept whatever the
    /// eviction policy says. Balanced by `unpin` when that request is answered — otherwise a remainder
    /// dropped between the answer and the judgment would let a stale record be believed.
    fn pin(&mut self, hold: TxId);
    fn unpin(&mut self, hold: TxId);

    /// This much of the hold is spoken for; `resolves` when nothing will be left.
    fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool);
    /// The batch committed, so this reservation is no longer speculative. What the hold has left
    /// follows from the `Apply` the engine is sent, not from this call.
    fn release_reservation(&mut self, hold: TxId, amount: Amount);
    /// Give the reservation back after a failed commit.
    fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool);

    /// Eviction of what nothing in flight still needs, driven from the reactor's tick.
    fn maintain(&mut self) -> usize;
    fn overlay_len(&self) -> usize;
}

/// The **queued** contract: the sequencer sends and moves on, a full queue is backpressure rather
/// than a lost command, and replies come back later in each lane's seq order. Everything that has to
/// reach the store — a write, a lookup, a fence — travels this way, which is what keeps them in order
/// relative to each other.
///
/// It carries the inline half rather than replacing it: one component answers both, and which
/// contract a call belongs to says whether it can fail and whether it crosses a thread.
pub trait PendingPort: PendingOverlay {
    /// Takes `&mut self` because the sequencer's own overlay follows what it sends: a hold it has just
    /// been told to create has all of itself left, and one it has removed is gone.
    fn send(&mut self, command: PendingCommand) -> Result<(), PendingCommand>;
    fn poll(&self) -> Option<PendingReply>;

    /// What the engine has to say on its own, drained once a tick. The third direction on this port:
    /// `send` is the sequencer speaking, `poll` is the engine answering, and this is the engine
    /// speaking first. A method rather than a trait of its own, because one component answers all
    /// three and splitting them would be three traits for one seam.
    fn notices(&self) -> Option<PendingNotice>;

    /// Tells the engine whether the sequencer has room for more expiry voids. The fourth direction, and the
    /// only one that carries no work: advisory, one-way, in the shape `Backpressure` already uses. A stale
    /// read costs one wasted offer rather than a wrong answer.
    ///
    /// It exists because a declined expiry void is not free. The sweep is the only thing that retries one, so
    /// it re-offers whatever it has not seen land — and with nothing to pace that, a full backlog becomes a
    /// re-offer every round, each costing a slot and a lane place for the sequencer to decline again.
    /// Measured before it existed: 780,000 declines in a five-second run and p99.9 three times what it was,
    /// against 21 million admissions of half a million real requests. Rule 12 says a backlog reaching its
    /// limit pauses whatever fills it — this is that signal for the one backlog whose filler is the ledger
    /// rather than a client, so there is no client to refuse instead.
    fn set_wants_expiry(&mut self, wanted: bool);
}
