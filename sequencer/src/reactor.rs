mod apply;
mod intake;
mod judge;

use ledger_base::ports::{AccountPort, IdempotencyPort, PendingNotice, PendingPort, RaftPort};
use ledger_base::{
    AccountId, AcctHandle, Ack, AckOutcome, Amount, Clock, Consumer, Effect, EffectKind, Footprint,
    LedgerError, LogSink, LogStream, Producer, Request, SystemClock, TxId,
};

use crate::config::ReactorConfig;
use crate::log_kind::LogKind;
use crate::metrics::{Metrics, StageTimes};
use crate::rules::budget::BudgetCoverage;
use crate::rules::linked::LinkedChains;
use crate::state::batcher::Batcher;
use crate::state::cascade::Cascade;
use crate::state::expiry::ExpiryQueue;
use crate::state::lane::{LaneState, LaneTable};
use crate::state::outbox::Outbox;
use crate::state::pending::PendingChannel;
use crate::state::pipeline::Pipeline;
use crate::state::pipeline::SlotId;
use crate::state::safety::Safety;

/// The sequencer's ends of the client queues. Creating them is the caller's business.
pub struct Transport {
    pub requests: Consumer<Request>,
    pub acks: Producer<Ack>,
}

/// What a rate limiter in front of the sequencer needs to see.
#[derive(Debug, Clone, Copy)]
pub struct Backpressure {
    /// True while a backlog is full and no new request is being admitted.
    pub intake_paused: bool,
    /// Acks the client has not taken yet.
    pub acks_queued: usize,
    /// Committed hold decisions the pending engine has not taken yet.
    pub pending_writes: usize,
    /// Proposals consensus has not answered yet.
    pub batches_in_flight: usize,
    /// Work slots in use, which is how many requests are somewhere in the pipeline.
    pub requests_in_flight: usize,
}

/// What must be true about the sequencer's own bookkeeping. A broken one is not a bad request: it
/// says this node can no longer account for what it did, which is the same class of failure as a
/// commit that cannot be paired with its batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broken {
    AppliedMoreThanJudged = 1,
    AccountViewDisagrees = 2,
    MoreRequestsThanSlots = 3,
    MoreProposalsThanAllowed = 4,
    /// Both sums of double-entry no longer match.
    IdentitiesBroken = 5,
    /// Nothing is in flight, so no availability may still be promised.
    OverlaySurvivedQuiescence = 6,
    /// A lane's own flag and the list that decides fail-stop disagree about quarantine.
    QuarantineDisagrees = 7,
}

/// Single-threaded non-blocking reactor. Every tick advances each stage as far as it can and
/// never waits for a specific completion; the only core CPU spent is the seq check, the judge
/// and the in-order apply.
pub struct Reactor<A: AccountPort, P: PendingPort, I: IdempotencyPort, R: RaftPort, C = SystemClock>
{
    config: ReactorConfig,
    clock: C,
    accounts: A,
    /// Order, overlay and quarantine, per account.
    lanes: LaneTable,
    /// Chains being assembled, judged, and the requests gated behind them.
    linked: LinkedChains,
    /// What the chain being judged resolves of each budget group it touches.
    budgets: BudgetCoverage,
    /// The intake queue, the work slots, and the dispatches a full queue refused.
    pipeline: Pipeline,
    /// Effects waiting to be proposed, and proposals consensus has not answered.
    batcher: Batcher,
    /// Acks on their way to the client, bounded so a slow client becomes backpressure.
    outbox: Outbox,
    /// Expiry voids waiting their turn, which comes after every client request this tick.
    expiry: ExpiryQueue,
    /// Chains freed by a gate that has just opened, so a cascade of them is a loop and not a stack.
    cascade: Cascade,
    /// The pending port and the committed decisions not yet handed to it; a queued write must
    /// precede any later lookup.
    pending: PendingChannel<P>,
    idem: I,
    raft: R,
    safety: Safety,
    log: LogSink,
    /// Kept so a pause is logged on the edge, not every tick.
    intake_paused: bool,
    /// Set on shutdown: nothing new is admitted, everything in flight finishes.
    intake_closed: bool,
    metrics: Metrics,
    stages: StageTimes,
}

/// A monotonic system clock; a simulation uses `with_clock`.
impl<A, P, I, R> Reactor<A, P, I, R, SystemClock>
where
    A: AccountPort,
    P: PendingPort,
    I: IdempotencyPort,
    R: RaftPort,
{
    pub fn new(
        config: ReactorConfig,
        transport: Transport,
        accounts: A,
        pending: P,
        idem: I,
        raft: R,
    ) -> Result<(Self, LogStream), LedgerError> {
        Self::with_clock(
            config,
            transport,
            accounts,
            pending,
            idem,
            raft,
            SystemClock::new(),
        )
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
    pub fn with_clock(
        config: ReactorConfig,
        transport: Transport,
        accounts: A,
        pending: P,
        idem: I,
        raft: R,
        clock: C,
    ) -> Result<(Self, LogStream), LedgerError> {
        config.validate()?;
        let capacity = config.capacity;
        let (mut log, stream) = ledger_base::log_channel(capacity.log_events);
        log.record(
            LogKind::STARTED,
            clock.now_nanos(),
            capacity.slots as u64,
            config.batching.size as u64,
        );
        let reactor = Self {
            clock,
            accounts,
            lanes: LaneTable::with_capacity(capacity.slots),
            linked: LinkedChains::new(config.linked.max_legs, config.batching.in_flight + 1),
            budgets: BudgetCoverage::new(config.linked.max_legs),
            pipeline: Pipeline::new(transport.requests, capacity.slots, capacity.intake_per_tick),
            batcher: Batcher::new(config.batching, config.batch_headroom()),
            outbox: Outbox::new(transport.acks, capacity.ack_backlog, capacity.slots),
            expiry: ExpiryQueue::new(capacity.expiry_backlog),
            cascade: Cascade::with_capacity(capacity.slots),
            pending: PendingChannel::new(
                pending,
                capacity.pending_write_backlog,
                capacity.pending_write_backlog,
            ),
            idem,
            raft,
            safety: Safety::new(config.safety),
            log,
            intake_paused: false,
            intake_closed: false,
            metrics: Metrics::default(),
            stages: StageTimes::default(),
            config,
        };
        Ok((reactor, stream))
    }

    pub fn tick(&mut self) -> bool {
        self.metrics.ticks += 1;
        let progress = if self.config.profile {
            self.timed_stages()
        } else {
            self.stages()
        };
        if !progress {
            self.metrics.idle_ticks += 1;
        }
        if let Err(broken) = self.counts_add_up() {
            self.on_broken(broken);
        }
        progress
    }

    /// Checked after every tick, so it has to be constant time: counters one stage keeps, against
    /// counters its peers keep. The whole-ledger version is `audit`.
    fn counts_add_up(&self) -> Result<(), Broken> {
        if self.metrics.committed > self.metrics.judged {
            return Err(Broken::AppliedMoreThanJudged);
        }
        if self.accounts.applied() != self.metrics.committed {
            return Err(Broken::AccountViewDisagrees);
        }
        if self.pipeline.in_flight() > self.config.capacity.slots {
            return Err(Broken::MoreRequestsThanSlots);
        }
        if self.batcher.in_flight_len() > self.config.batching.in_flight {
            return Err(Broken::MoreProposalsThanAllowed);
        }
        Ok(())
    }

    /// Everything the constant-time check cannot afford: it walks every account and every
    /// quarantined lane, so it belongs between ticks, in a test or a simulation.
    pub fn audit(&self) -> Result<(), Broken> {
        self.counts_add_up()?;
        if !self.accounts.totals().balanced() {
            return Err(Broken::IdentitiesBroken);
        }
        if self.is_quiescent() && self.lanes.overlay_total() != 0 {
            return Err(Broken::OverlaySurvivedQuiescence);
        }
        for id in self.safety.quarantined() {
            let flagged = self
                .accounts
                .resolve(*id)
                .is_some_and(|handle| self.lanes.get(handle).is_quarantined());
            if !flagged {
                return Err(Broken::QuarantineDisagrees);
            }
        }
        Ok(())
    }

    /// A broken invariant is loud in a test and fatal in a release: nothing more is applied, because
    /// the numbers the next effect would be applied against no longer add up.
    fn on_broken(&mut self, broken: Broken) {
        debug_assert!(false, "broken invariant: {broken:?}");
        self.metrics.invariant_breaks += 1;
        if self.safety.seal_applies() {
            self.record(LogKind::INVARIANT_BROKEN, broken as u64, self.metrics.ticks);
        }
    }

    fn stages(&mut self) -> bool {
        let mut progress = self.drain_backlogs();
        progress |= self.intake();
        progress |= self.drain_replies();
        progress |= self.propose();
        progress |= self.apply();
        self.evict_idle_holds();
        progress
    }

    /// Same stages, each one timed. Only the differences are kept, so the clock's own cost lands
    /// in whichever stage follows it rather than inflating the total.
    fn timed_stages(&mut self) -> bool {
        let opened = self.clock.now_nanos();
        let mut progress = self.drain_backlogs();
        let backlog_done = self.clock.now_nanos();
        progress |= self.intake();
        let intake_done = self.clock.now_nanos();
        progress |= self.drain_replies();
        let judge_done = self.clock.now_nanos();
        progress |= self.propose();
        let propose_done = self.clock.now_nanos();
        progress |= self.apply();
        let apply_done = self.clock.now_nanos();
        self.evict_idle_holds();
        self.stages.backlog += backlog_done.saturating_sub(opened);
        self.stages.intake += intake_done.saturating_sub(backlog_done);
        self.stages.judge += judge_done.saturating_sub(intake_done);
        self.stages.propose += propose_done.saturating_sub(judge_done);
        self.stages.apply += apply_done.saturating_sub(propose_done);
        progress
    }

    /// The log sink counts what it had to drop, so the count is read here rather than kept twice.
    pub fn metrics(&self) -> Metrics {
        Metrics {
            log_drops: self.log.dropped(),
            ..self.metrics
        }
    }

    /// Zero unless the run asked for profiling.
    pub fn stage_times(&self) -> StageTimes {
        self.stages
    }

    /// What the sequencer is holding. Almost all of it is *in flight* rather than working set — slots
    /// held for a round trip, effects waiting on consensus, answers a client has not taken — so its
    /// size follows the component latencies this run was given, which is the part of a sizing answer
    /// no multiplication of the account count produces. The lane table is the exception: one entry per
    /// account, and it only grows.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        self.pipeline.footprint(&mut footprint);
        self.lanes.footprint(&mut footprint);
        self.batcher.footprint(&mut footprint);
        self.outbox.footprint(&mut footprint);
        self.pending.footprint(&mut footprint);
        footprint
    }

    /// The most work slots ever held at once. Against the pool's size, this is how much of the
    /// sequencer's own capacity the run actually needed.
    pub fn peak_in_flight(&self) -> usize {
        self.pipeline.peak_in_flight()
    }

    pub fn accounts(&self) -> &A {
        &self.accounts
    }

    /// One account's lane, for a test that has to say *why* nothing is moving. A stall shows up here and
    /// almost nowhere else: a seq issued and never judged stops everything behind it, and the counters a
    /// report prints cannot tell that from a lane with nothing to do.
    pub fn lane(&self, account: AccountId) -> Option<&LaneState> {
        self.accounts
            .resolve(account)
            .and_then(|at| self.lanes.try_get(at))
    }

    pub fn raft(&self) -> &R {
        &self.raft
    }

    /// The two components a run has to ask for their own occupancy, since theirs lives on their own
    /// threads and the sequencer only holds a handle.
    pub fn pending(&self) -> &P {
        self.pending.port()
    }

    pub fn idem(&self) -> &I {
        &self.idem
    }

    /// Zero once nothing is in flight.
    pub fn overlay_total(&self) -> Amount {
        self.lanes.overlay_total()
    }

    pub fn is_fail_stopped(&self) -> bool {
        self.safety.is_fail_stopped()
    }

    /// Whether this node has stopped applying. A sealed apply path is a legitimate end for a run and an
    /// illegitimate one for a test that is waiting on money to move, so a test that waits has to be able
    /// to tell the two apart rather than time out on both.
    pub fn applies_sealed(&self) -> bool {
        self.safety.applies_sealed()
    }

    pub fn quarantined(&self) -> &[AccountId] {
        self.safety.quarantined()
    }

    /// Stops admitting. What is in the pipeline still finishes.
    pub fn close_intake(&mut self) {
        self.intake_closed = true;
    }

    /// True when nothing is in flight and nothing is waiting to be handed over.
    pub fn is_quiescent(&self) -> bool {
        self.pipeline.in_flight() == 0
            && self.batcher.in_flight_len() == 0
            && self.outbox.depth() == 0
            && self.pending.depth() == 0
    }

    pub fn backpressure(&self) -> Backpressure {
        Backpressure {
            intake_paused: self.intake_paused,
            acks_queued: self.outbox.depth(),
            pending_writes: self.pending.depth(),
            batches_in_flight: self.batcher.in_flight_len(),
            requests_in_flight: self.pipeline.in_flight(),
        }
    }

    /// Operator action, once the lane has drained.
    pub fn release_quarantine(&mut self, id: AccountId) -> Result<(), LedgerError> {
        let handle = self
            .accounts
            .resolve(id)
            .ok_or(LedgerError::UnknownAccount(id))?;
        self.lanes.get_mut(handle).release_quarantine()?;
        self.safety.release(id);
        self.record(LogKind::LANE_RELEASED, id.raw(), 0);
        Ok(())
    }

    /// Refused while consensus still owes answers, which must still be applied in order.
    pub fn clear_fail_stop(&mut self) -> Result<(), LedgerError> {
        if self.batcher.in_flight_len() > 0 {
            return Err(LedgerError::QuarantineDraining);
        }
        self.safety.clear_fail_stop();
        Ok(())
    }

    pub fn drain_backlogs(&mut self) -> bool {
        // Before anything else this tick, including the apply below: the only notice there is seals the
        // apply path, and a seal decided now has to be in effect before this tick applies anything.
        let mut progress = self.drain_pending_notices();
        progress |= self.outbox.flush();
        progress |= self.pending.flush();
        while let Some(slot) = self.pipeline.deferred_front() {
            if !self.dispatch(slot) {
                break;
            }
            self.pipeline.deferred_done();
            progress = true;
        }
        progress
    }

    /// The one place the engine speaks first. Drained in full and before every other stage, because the
    /// seal is on this wire: a seal decided now has to be in effect before this tick applies anything, and
    /// it may not wait behind a void.
    ///
    /// The two notices are not the same character of work, so only one of them acts here. A seal happens
    /// once in the life of a node and stops it. An expiry void is ordinary traffic nobody is waiting for —
    /// millions of them in a run — so it is parked and admitted after the clients, on its own budget.
    /// Acting on it here was giving the ledger's own background work first call on every slot.
    fn drain_pending_notices(&mut self) -> bool {
        let mut progress = false;
        while let Some(notice) = self.pending.notice() {
            progress = true;
            match notice {
                PendingNotice::HoldNotStored { hold } => self.on_hold_not_stored(hold),
                PendingNotice::HoldExpired { void } => {
                    if !self.expiry.park(void) {
                        self.metrics.expiry_dropped += 1;
                    }
                }
            }
        }
        progress
    }

    /// A hold consensus committed that the engine could not store. Its columns have already moved and
    /// its client has already been told it committed — neither can be taken back — but the pending
    /// column that hold reserved can now never come down, because no resolution of a hold the store
    /// does not have can be answered. So this node's state has stopped following the log, which is the
    /// same class of failure as a committed effect that cannot be applied: the apply path is sealed and
    /// the drain that never completes is the signal to replace the leader.
    ///
    /// There is deliberately no operator action. The index is sized from a declared maximum, so passing
    /// it is a business change — the remedy is a configuration change and a rolling restart, and the
    /// index is rebuilt on the way back up anyway.
    fn on_hold_not_stored(&mut self, hold: TxId) {
        self.metrics.holds_not_stored += 1;
        if self.safety.seal_applies() {
            // The low 64 bits of the id: a log event carries two numbers, and this one is a diagnostic
            // pointing at which hold, not a key anything is looked up by.
            self.record(
                LogKind::HOLD_NOT_STORED,
                hold.raw() as u64,
                self.metrics.committed,
            );
        }
    }

    /// The engine answered from state older than a decision it had already been handed. The same kind of
    /// fault as a seq gap — an external component is broken and our own state is intact — so it gets the
    /// same treatment. Counted separately because it is the check that stands in for the lane's order on
    /// requests that keep no place in it, and a run has to be able to say which of the two fired.
    pub fn on_stale_answer(&mut self, lane: AccountId, handle: AcctHandle) {
        self.metrics.stale_answers += 1;
        self.record(LogKind::SEQ_GAP, lane.raw(), 0);
        self.quarantine_lane(lane, handle);
    }

    pub fn on_seq_gap(&mut self, lane: AccountId, handle: AcctHandle, seq: u64) {
        self.metrics.seq_gaps += 1;
        self.record(LogKind::SEQ_GAP, lane.raw(), seq);
        self.quarantine_lane(lane, handle);
    }

    /// What a broken component costs a lane, whichever check found it: the lane stops serving and the
    /// rest keeps going, and enough lanes at once means the component rather than the lane.
    fn quarantine_lane(&mut self, lane: AccountId, handle: AcctHandle) {
        // One fact in two places: the flag the hot path reads and the list that decides fail-stop.
        self.lanes.get_mut(handle).quarantine();
        if self.safety.quarantine(lane) {
            self.metrics.quarantines += 1;
            self.record(LogKind::LANE_QUARANTINED, lane.raw(), 0);
        }
        // Several lanes at once means the component, not one lane.
        if self.safety.lanes_lost() {
            self.trip_fail_stop();
        }
    }

    pub fn trip_fail_stop(&mut self) {
        if self.safety.fail_stop() {
            let lanes = self.safety.quarantined().len() as u64;
            self.record(LogKind::FAIL_STOP, lanes, 0);
        }
    }

    pub fn track_intake_pause(&mut self, paused: bool) {
        if paused == self.intake_paused {
            return;
        }
        self.intake_paused = paused;
        if paused {
            self.metrics.intake_pauses += 1;
            let acks = self.outbox.depth() as u64;
            let writes = self.pending.depth() as u64;
            self.record(LogKind::INTAKE_PAUSED, acks, writes);
        } else {
            self.record(LogKind::INTAKE_RESUMED, 0, 0);
        }
    }

    pub fn finish(&mut self, slot: SlotId, outcome: AckOutcome) {
        let item = *self.pipeline.item(slot);
        if item.holds_pin() {
            self.pending.unpin(item.tx.pending_ref);
        }
        self.pipeline.release(slot);
        self.lanes.get_mut(item.debit).left();
        match outcome {
            AckOutcome::Rejected(_) => self.metrics.rejected += 1,
            AckOutcome::Duplicate => self.metrics.duplicates += 1,
            AckOutcome::Committed => {}
        }
        // The ledger does not answer itself. An expiry void was nobody's request, so an ack for it would
        // put a transaction id no client sent into the client's stream.
        if !item.kind.is_client() {
            return;
        }
        self.outbox.emit(Ack {
            tx_id: item.tx.id,
            lane: item.lane,
            seq: item.seq,
            outcome,
            submitted_at_nanos: item.submitted_at_nanos,
        });
    }

    fn reject_before_seq(&mut self, request: &Request, err: LedgerError) {
        self.metrics.rejected += 1;
        self.outbox.emit(Ack {
            tx_id: request.tx.id,
            lane: request.tx.lane(),
            seq: 0,
            outcome: AckOutcome::Rejected(err),
            submitted_at_nanos: request.submitted_at_nanos,
        });
    }

    /// Both overlays are taken together: availability the lane promises, and the hold remainder the
    /// engine sets aside. One without the other leaks a promise nobody will give back.
    pub fn take_overlays(&mut self, effect: &Effect) {
        self.lanes.reserve(effect);
        self.reserve_hold(effect);
    }

    /// The decision did not survive, so both are given back.
    pub fn give_back_overlays(&mut self, effect: &Effect) {
        self.lanes.release(effect);
        self.compensate_hold(effect);
    }

    /// The decision committed, which is not symmetrical: the lane's promise is released, while the
    /// hold's reservation stops being speculative and what the hold has left now follows the write
    /// the engine is sent.
    pub fn settle_overlays(&mut self, effect: &Effect) {
        self.lanes.release(effect);
        if matches!(effect.kind, EffectKind::Settle | EffectKind::Void) {
            self.pending
                .release_reservation(effect.pending_ref, effect.amount);
        }
    }

    /// State transitions only, never per request.
    pub fn record(&mut self, kind: u16, a: u64, b: u64) {
        let at = self.clock.now_nanos();
        self.log.record(kind, at, a, b);
    }
}
