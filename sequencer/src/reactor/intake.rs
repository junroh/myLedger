use ledger_base::ports::{AccountPort, IdempotencyPort, PendingPort, RaftPort};
use ledger_base::Clock;

use super::Reactor;
use ledger_base::ports::{IdemAsk, IdemRequest, PendingFence, PendingLookup};
use ledger_base::{LedgerError, LinkedChainId, Request, Transfer, TransferFlags, UNORDERED};

use crate::state::pipeline::{DepFlags, SlotId, SlotPool, WorkItem};

/// What the pending path has to do for one request. Deciding it is the subtle part and causes
/// nothing, so it is stated on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingStep {
    /// Fetch the record this request is judged by. Every resolution does, because the record is the
    /// engine's: the sequencer keeps only what it has decided about the hold, never what the hold is.
    Lookup,
    /// Read nothing, but take a place in the lane's queue, or this request would overtake the
    /// replies already outstanding on that lane.
    Fence,
    /// Judge it without leaving the reactor.
    Inline,
}

impl PendingStep {
    /// `needs_record` is true for a resolution whose hold the engine has not already said is missing —
    /// the one answer that needs no record.
    pub(super) fn of(needs_record: bool, lane_waiting: bool) -> Self {
        match (needs_record, lane_waiting) {
            (true, _) => Self::Lookup,
            (false, true) => Self::Fence,
            (false, false) => Self::Inline,
        }
    }
}

impl<A, P, I, R, C> Reactor<A, P, I, R, C>
where
    A: AccountPort,
    P: PendingPort,
    I: IdempotencyPort,
    R: RaftPort,
    C: Clock,
{
    /// S1: issue the lane seq, S2: throw the external calls without waiting.
    pub fn intake(&mut self) -> bool {
        if self.intake_closed {
            return false;
        }
        let paused = self.pipeline.has_deferred()
            || self.outbox.is_saturated()
            || self.pending.is_saturated()
            // Judged effects waiting for consensus are a backlog like any other: bounded, and reaching
            // the bound slows the client down instead of growing memory here.
            || self.batcher.is_saturated();
        self.track_intake_pause(paused);
        if paused {
            return false;
        }

        let mut progress = false;
        for _ in 0..self.config.capacity.intake_per_tick {
            let Some(request) = self.pipeline.next_request() else {
                break;
            };
            progress = true;
            self.metrics.admitted += 1;
            self.admit(&request);
            if request.end_of_batch {
                if let Some(chain) = self.linked.close_batch() {
                    self.judge_chain(chain);
                }
            }
            if self.pipeline.has_deferred() {
                break;
            }
        }
        // After every client this tick was offered a slot, never before: the ledger's own work goes behind
        // the work someone is waiting for. Nothing about correctness rests on this order — every ordered
        // request shares the queue that puts a lane back together — so this is the priority alone.
        progress | self.admit_parked_expiry()
    }

    /// The parked voids, up to this tick's budget. The budget is a share and not a threshold on purpose: a
    /// tick that admits anything admits some of these, so the sweep cannot be starved to a standstill by
    /// traffic that never lets up — which is what bounds how far behind a day can fall.
    ///
    /// Nothing here has to look at the lane. A void is ordered by the same queue every client request is
    /// ordered by — it asks that component for its place and not for a verdict — so being admitted late in
    /// the tick costs it nothing but its turn.
    fn admit_parked_expiry(&mut self) -> bool {
        let mut progress = false;
        for _ in 0..self.config.capacity.expiry_per_tick {
            if self.pipeline.has_deferred() {
                break;
            }
            let Some(void) = self.expiry.take() else {
                break;
            };
            progress = true;
            self.admit_expiry(void);
        }
        progress
    }

    fn admit(&mut self, request: &Request) {
        // The client boundary, and the only place this rule belongs: the top bit of a transaction id is
        // the ledger's own, so a client using it could collide with a resolution the ledger derives and
        // idempotency would answer a real transfer as a duplicate. Not part of `Transfer::validate`,
        // which is shape only — a ledger-origin id is perfectly well shaped, and the ledger submits one.
        //
        // Asked of the id and not of the kind, deliberately. `TransferKind` reads that bit too, but only
        // to tell the two voids apart, so a client sending a reserved id with *settle* flags would come
        // back a perfectly ordinary `Settle`. What is being policed here is the id space itself.
        if request.tx.id.is_ledger_origin() {
            return self.reject_before_seq(request, LedgerError::ReservedTransactionId);
        }
        let chain_id = self
            .linked
            .admit(request.tx.flags.contains(TransferFlags::LINKED));
        match self.prepare(request, chain_id) {
            Ok(slot) => {
                if let Some(id) = chain_id {
                    let lane = self.pipeline.item(slot).debit;
                    self.linked.add_leg(id, lane, slot);
                }
                self.dispatch_or_defer(slot);
            }
            Err(err) => {
                self.reject_before_seq(request, err);
                // One bad leg dooms the chain: it commits whole or not at all.
                if let Some(id) = chain_id {
                    self.linked.fail(id, err);
                    if let Some(chain) = self.linked.settle_if_complete(id) {
                        self.judge_chain(chain);
                    }
                }
            }
        }
    }

    /// A resolution the engine asked for rather than a client: the hold's retention has run out and what
    /// is left of it has to be released, or the pending column it reserved never comes back down.
    ///
    /// It is judged exactly like a client's resolution, and it has to be: a settle the client submitted
    /// may be in flight for the same hold, and only the judge sees both. So it takes a slot, a place in
    /// its lane and a lookup, and it is refused by the same rules — a hold already resolved, or a lane
    /// quarantined, and the sweep will offer it again when it comes round.
    pub(super) fn admit_expiry(&mut self, void: Transfer) {
        let request = Request::single(void, self.clock.now_nanos());
        match self.prepare(&request, None) {
            Ok(slot) => {
                self.metrics.holds_expired += 1;
                self.dispatch_or_defer(slot);
            }
            // Nobody to tell: no client asked for this. The sweep offers it again next round, and if the
            // reason was terminal — a sealed apply path — nothing is being released anyway.
            Err(_) => self.metrics.expiry_refused += 1,
        }
    }

    fn dispatch_or_defer(&mut self, slot: SlotId) {
        if !self.dispatch(slot) {
            self.pipeline.defer(slot);
            self.metrics.dispatch_deferred += 1;
        }
    }

    fn prepare(
        &mut self,
        request: &Request,
        chain: Option<LinkedChainId>,
    ) -> Result<SlotId, LedgerError> {
        if self.safety.is_fail_stopped() {
            return Err(LedgerError::FailStop);
        }
        let tx_kind = request.tx.validate()?;
        let debit_account = self
            .accounts
            .resolve(request.tx.debit_account)
            .ok_or(LedgerError::UnknownAccount(request.tx.debit_account))?;
        let credit_account = self
            .accounts
            .resolve(request.tx.credit_account)
            .ok_or(LedgerError::UnknownAccount(request.tx.credit_account))?;
        if self.accounts.record(debit_account).ledger() != request.tx.ledger
            || self.accounts.record(credit_account).ledger() != request.tx.ledger
        {
            return Err(LedgerError::LedgerMismatch);
        }
        if self.lanes.get_mut(debit_account).is_quarantined() {
            return Err(LedgerError::AccountQuarantined(request.tx.debit_account));
        }

        // A request has a place in its lane's order when its judgment can depend on the lane's earlier
        // requests, and that comes from one thing: the balance. An account the ledger does not constrain
        // has no balance to protect, so nothing debiting it depends on what came before — no seq, no
        // continuity check, no fence, and no place for a later request to queue behind.
        //
        // A resolution used to be ordered whatever it debited. What the lane bought it was never safety:
        // double resolution is prevented per *hold*, by the overlay's reservation and its `resolved`
        // flag, and a resolution judged with no record at all is rejected rather than accepted. What the
        // lane bought was which of two concurrent resolutions of one hold wins — and one still wins
        // either way. What it cost was a lane thirteen thousand deep on a clearing account, measured with
        // `--external-ratio 30 --resolve-after 100000`. Design notes §1 carries both halves.
        let reads_hold = tx_kind.needs_pending_lookup();
        let ordered = self.accounts.record(debit_account).is_constrained();
        // A request that has a place in the lane keeps it: while the lane waits on the pending
        // path, this one waits there too, or it would overtake what is ahead of it.
        let needs_pending_reply =
            reads_hold || (ordered && self.lanes.get(debit_account).awaits_pending_reply());
        // Only an ordered reply is counted, because the counter is what decides whether a later request
        // must fence — and nothing needs to queue behind a reply that holds no place.
        if ordered && needs_pending_reply && !self.lanes.get_mut(debit_account).has_reply_capacity()
        {
            return Err(LedgerError::Overloaded);
        }
        let Some(slot) = self.pipeline.alloc() else {
            self.metrics.slot_exhaustion += 1;
            return Err(LedgerError::Overloaded);
        };

        let mut deps = if needs_pending_reply {
            DepFlags::IDEM.with(DepFlags::PENDING)
        } else {
            DepFlags::IDEM
        };
        // Every ordered request keeps its idempotency dependency, the ledger's own resolutions included —
        // but they ask for something different once they get there (`IdemAsk::Serialize`). Dropping the
        // dependency was the mistake: it did avoid the map, which a derived id must, and it also left the
        // request out of the queue that puts a lane back in order. A resolution waiting only on the engine
        // and a transfer waiting only on dedup then had nothing ordering them, and whichever component
        // answered first was judged first — a seq gap, and a lane quarantined for a fault of ours.
        let gate = self
            .linked
            .gate_for(debit_account)
            .filter(|gate| Some(*gate) != chain);
        if gate.is_some() {
            deps = deps.with(DepFlags::LINKED_CHAIN);
        }
        let lane_state = self.lanes.get_mut(debit_account);
        lane_state.entered();
        let seq = if ordered {
            lane_state.issue_seq()
        } else {
            self.metrics.order_exempt += 1;
            UNORDERED
        };
        *self.pipeline.item_mut(slot) = WorkItem {
            tx: request.tx,
            digest: request.tx.digest(),
            seq,
            lane: request.tx.lane(),
            kind: tx_kind,
            debit: debit_account,
            credit: credit_account,
            chain: chain.unwrap_or(LinkedChainId::ABSENT),
            deps,
            sent: DepFlags::NONE,
            verdict: None,
            submitted_at_nanos: request.submitted_at_nanos,
        };
        if let Some(gate) = gate {
            self.linked.wait_behind(gate, slot);
            self.metrics.lane_gated += 1;
        }
        Ok(slot)
    }

    /// Returns false when an external queue is full: the item keeps its seq and is retried,
    /// because dropping it would leave a permanent gap in its lane. Nothing here waits.
    pub(super) fn dispatch(&mut self, slot_id: SlotId) -> bool {
        let item = *self.pipeline.item(slot_id);
        let mut sent = item.sent;
        if item.deps.contains(DepFlags::IDEM) && !sent.contains(DepFlags::IDEM) {
            if !self.send_idem(slot_id, &item) {
                self.pipeline.item_mut(slot_id).sent = sent;
                return false;
            }
            sent = sent.with(DepFlags::IDEM);
        }
        if item.deps.contains(DepFlags::PENDING) && !sent.contains(DepFlags::PENDING) {
            if self.pending.blocks_lookups() || !self.take_pending_step(slot_id, &item) {
                self.pipeline.item_mut(slot_id).sent = sent;
                return false;
            }
            sent = sent.with(DepFlags::PENDING);
            // The pin is taken in the same statement that records the step as sent, so `holds_pin`
            // reads one fact rather than re-deriving when a pin was owed. This request reads the hold
            // when it is judged, so the engine keeps it whatever its eviction policy says, until the
            // request is answered.
            if item.kind.needs_pending_lookup() {
                self.pending.pin(item.tx.pending_ref);
            }
        }
        let judgeable = {
            let item = self.pipeline.item_mut(slot_id);
            item.sent = sent;
            item.is_judgeable()
        };
        if judgeable {
            self.on_ready(slot_id);
        }
        true
    }

    fn send_idem(&mut self, slot: SlotId, item: &WorkItem) -> bool {
        let request = IdemRequest {
            correlation: SlotPool::correlation(slot),
            tx_id: item.tx.id,
            lane: item.lane,
            seq: item.seq,
            digest: item.digest,
            // An expiry void asks for the queue and not the map. Why it may not be recorded is on
            // `IdemAsk::Serialize`; why it is here at all is the queue — a lane is put back into order by
            // the component the request travels through, and one that travelled through neither had
            // nothing ordering it against the requests that did.
            ask: match item.kind.is_client() {
                true => IdemAsk::Check,
                false => IdemAsk::Serialize,
            },
        };
        self.idem.dispatch(request).is_ok()
    }

    /// Decides what the pending path owes this request and does it. False means an external queue
    /// refused: nothing has been counted or pinned, so the retry is clean.
    fn take_pending_step(&mut self, slot: SlotId, item: &WorkItem) -> bool {
        let needs_record =
            item.kind.needs_pending_lookup() && !self.pending.hold_is_missing(item.tx.pending_ref);
        let waiting = self.lanes.get(item.debit).awaits_pending_reply();
        match PendingStep::of(needs_record, waiting) {
            PendingStep::Lookup => {
                let lookup = PendingLookup {
                    correlation: SlotPool::correlation(slot),
                    tx_id: item.tx.id,
                    lane: item.lane,
                    seq: item.seq,
                    pending_ref: item.tx.pending_ref,
                };
                if !self.pending.lookup(lookup) {
                    return false;
                }
                self.pending.begin_lookup(item.tx.pending_ref);
                // The answer has to reflect every decision the engine has already been given. Recorded
                // now, because now is when "already" is defined.
                self.pipeline
                    .expect_applies(slot, self.pending.applies_sent());
                if item.keeps_lane_place() {
                    self.lanes.get_mut(item.debit).expect_pending_reply();
                }
                self.metrics.pending_lookups += 1;
            }
            PendingStep::Fence => {
                let fence = PendingFence {
                    correlation: SlotPool::correlation(slot),
                    lane: item.lane,
                    seq: item.seq,
                };
                if !self.pending.fence(fence) {
                    return false;
                }
                self.lanes.get_mut(item.debit).expect_pending_reply();
                self.metrics.fences += 1;
            }
            PendingStep::Inline => self.pipeline.item_mut(slot).clear_dep(DepFlags::PENDING),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole rule of the pending path: a resolution fetches the record it is judged by, whatever
    /// its lane is doing, because that record is the engine's and this request has none. Everything
    /// else reads nothing and only needs a place in its lane's order when the lane is waiting — which
    /// includes the one resolution that needs no record, of a hold the engine has already said is
    /// not there.
    #[test]
    fn the_pending_step_follows_the_hold_and_the_lane() {
        let waiting = true;
        let idle = false;
        let needs_record = true;

        assert_eq!(PendingStep::of(needs_record, idle), PendingStep::Lookup);
        assert_eq!(PendingStep::of(needs_record, waiting), PendingStep::Lookup);

        assert_eq!(PendingStep::of(!needs_record, idle), PendingStep::Inline);
        assert_eq!(PendingStep::of(!needs_record, waiting), PendingStep::Fence);
    }
}
