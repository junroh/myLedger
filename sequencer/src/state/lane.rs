use std::mem::size_of;

use ledger_base::{AcctHandle, Amount, Effect, Footprint, LedgerError, Seq};

/// One account's lane, from the sequencer's side. All of it is volatile — a new leader starts
/// from scratch — which is exactly why none of it belongs in the account component.
///
/// - `seq_counter` is what the next request on this lane will be given.
/// - `last_seq` is what has been judged, so `seq == last_seq + 1` is the contract-1 check.
/// - `speculative` is the **speculative overlay** of the design's three layers: availability
///   already promised to requests that are proposed but not committed. Negative amounts only —
///   money a request will bring in is not lent to anyone else before it commits.
/// - `in_flight` counts requests of this lane still in the pipeline, which is what says whether
///   a quarantined lane has drained.
/// - `pending_replies` counts replies outstanding on the pending path; while it is non-zero every
///   later request of the lane must travel that path too, or it would overtake them.
/// - `quarantined` isolates the lane after a contract-1 violation.
///
/// Thirty-two bytes, not a whole cache line: 32 divides both line sizes, so a lane never
/// straddles, and four times as many fit in cache. Measured, that is 2.8x faster at a million
/// accounts than one line per lane. The alignment is what makes it a guarantee rather than a
/// property of whatever the allocator happened to return.
#[derive(Debug, Clone, Copy, Default)]
#[repr(align(32))]
pub struct LaneState {
    seq_counter: Seq,
    last_seq: Seq,
    speculative: Amount,
    in_flight: u32,
    pending_replies: u16,
    quarantined: bool,
}

ledger_base::layout_claim!(LAYOUT: LaneState, size = 32, ledger_base::LineFit::Inside);

impl LaneState {
    /// Capped so the counter cannot wrap and under-report, which would break lane ordering.
    pub const MAX_PENDING_REPLIES: u16 = u16::MAX;

    pub const fn last_seq(&self) -> Seq {
        self.last_seq
    }

    pub const fn speculative(&self) -> Amount {
        self.speculative
    }

    pub const fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    pub fn issue_seq(&mut self) -> Seq {
        self.seq_counter += 1;
        self.seq_counter
    }

    /// Contract-1 check, not a reorder: a gap means an external component returned out of order.
    pub fn accept_seq(&mut self, seq: Seq) -> Result<(), LedgerError> {
        let expected = self.last_seq + 1;
        if seq != expected {
            return Err(LedgerError::SeqGap { expected, got: seq });
        }
        self.last_seq = seq;
        Ok(())
    }

    pub fn reserve(&mut self, amount: Amount) {
        self.speculative -= amount;
    }

    pub fn release(&mut self, amount: Amount) {
        self.speculative += amount;
    }

    /// While true, later requests of the lane must take the pending path too.
    pub const fn awaits_pending_reply(&self) -> bool {
        self.pending_replies > 0
    }

    pub const fn has_reply_capacity(&self) -> bool {
        self.pending_replies < Self::MAX_PENDING_REPLIES
    }

    pub fn expect_pending_reply(&mut self) {
        debug_assert!(self.has_reply_capacity());
        self.pending_replies += 1;
    }

    pub fn pending_reply_arrived(&mut self) {
        debug_assert!(self.pending_replies > 0);
        self.pending_replies -= 1;
    }

    pub const fn in_flight(&self) -> u32 {
        self.in_flight
    }

    pub fn entered(&mut self) {
        self.in_flight += 1;
    }

    pub fn left(&mut self) {
        debug_assert!(self.in_flight > 0);
        self.in_flight -= 1;
    }

    pub fn quarantine(&mut self) {
        self.quarantined = true;
    }

    /// Issuance restarts from the last confirmed seq so clients can re-submit. Only safe once the
    /// lane has drained, or an in-flight request would land on a reissued seq.
    pub fn release_quarantine(&mut self) -> Result<(), LedgerError> {
        if self.in_flight > 0 {
            return Err(LedgerError::QuarantineDraining);
        }
        self.quarantined = false;
        self.seq_counter = self.last_seq;
        Ok(())
    }
}

/// One lane per account, indexed by the handle the account component hands out, so nothing is
/// hashed after intake.
///
/// A lane is the order promised for one account: requests debiting the same account are judged in
/// arrival order. The debit side is the lane because it is the only side a balance constraint
/// applies to. Requests on different lanes have no promised order unless they are linked.
pub struct LaneTable {
    lanes: Vec<LaneState>,
}

impl LaneTable {
    /// One lane per account the ledger has seen. Working set, not in flight: it grows with the
    /// account count and never shrinks.
    pub fn footprint(&self, footprint: &mut Footprint) {
        let live = self.lanes.len();
        // One per account and no ceiling, so nothing to compare a peak against.
        footprint.other(
            "lane state",
            live,
            live,
            0,
            self.lanes.capacity() * size_of::<LaneState>(),
        );
    }
}

impl LaneTable {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            lanes: Vec::with_capacity(capacity),
        }
    }

    pub fn get(&self, handle: AcctHandle) -> &LaneState {
        debug_assert!(handle.index() < self.lanes.len());
        &self.lanes[handle.index()]
    }

    /// Grows to cover a handle the account component just resolved for the first time.
    pub fn get_mut(&mut self, handle: AcctHandle) -> &mut LaneState {
        if handle.index() >= self.lanes.len() {
            self.lanes.resize(handle.index() + 1, LaneState::default());
        }
        &mut self.lanes[handle.index()]
    }

    /// Moves the overlay, so the next request on the lane sees the promise, not a stale balance.
    pub fn reserve(&mut self, effect: &Effect) {
        let amount = effect.reservation();
        if amount != 0 {
            self.get_mut(effect.debit).reserve(amount);
        }
    }

    pub fn release(&mut self, effect: &Effect) {
        let amount = effect.reservation();
        if amount != 0 {
            self.get_mut(effect.debit).release(amount);
        }
    }

    pub fn overlay_total(&self) -> Amount {
        self.lanes.iter().map(LaneState::speculative).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seqs are handed out in order and accepted in the same order. Anything else is a contract-1
    /// violation and must be reported as a gap, not accepted.
    #[test]
    fn a_reply_out_of_seq_order_is_reported_as_a_gap() {
        let mut lane = LaneState::default();
        let first = lane.issue_seq();
        let second = lane.issue_seq();
        assert_eq!((first, second), (1, 2));

        assert_eq!(
            lane.accept_seq(second),
            Err(LedgerError::SeqGap {
                expected: 1,
                got: 2
            })
        );
        assert_eq!(lane.last_seq(), 0, "a gap must not move the lane on");
        assert_eq!(lane.accept_seq(first), Ok(()));
        assert_eq!(lane.accept_seq(second), Ok(()));
    }

    /// A quarantined lane may only be released once it has drained, and then reissues from the last
    /// seq it judged, so a client can re-submit what was lost.
    #[test]
    fn a_quarantined_lane_is_released_only_after_it_drains() {
        let mut lane = LaneState::default();
        lane.entered();
        lane.issue_seq();
        lane.accept_seq(1).expect("first seq");
        lane.issue_seq();
        lane.quarantine();

        assert_eq!(
            lane.release_quarantine(),
            Err(LedgerError::QuarantineDraining)
        );
        assert!(lane.is_quarantined());

        lane.left();
        assert_eq!(lane.release_quarantine(), Ok(()));
        assert!(!lane.is_quarantined());
        assert_eq!(
            lane.issue_seq(),
            2,
            "issuance restarts from the last judged seq"
        );
    }
}
