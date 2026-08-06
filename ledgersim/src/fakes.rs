//! The three components, one thread and one clock short of the real ones. Each is a handle onto
//! shared state, so the simulation can drive it while the reactor owns a copy — the ports are traits
//! for exactly this reason, and nothing in the ledger changes to be simulated.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use ledger_base::ports::{
    IdemAsk, IdemReply, IdemRequest, IdemVerdict, IdempotencyPort, OverlayState, PendingCommand,
    PendingEffect, PendingNotice, PendingOverlay, PendingPort, PendingReply, RaftCommit,
    RaftOutcome, RaftPort, RaftProposal,
};
use ledger_base::{Amount, FxHashMap, Prng, Transfer, TxId, UNORDERED};
use ledger_pending::{
    HoldOverlay, MemBlockStore, PendingEngine, DEFAULT_FLUSH_BLOCKS, DEFAULT_RESIDENT_BLOCKS,
    DEFAULT_SLOTS,
};
use ledger_stubkit::{LaneOrderer, Server, ServerStats};

/// How fast each component answers, and how much it keeps. Nothing here misbehaves — a slow component
/// is a plan, not a fault, and keeping the two apart is what stopped `capacity` from having to fill in
/// half a struct with "off".
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// The pending engine as a black box: what it answers one command in. Whatever it does inside — an
    /// index, a cache, a disk tier — is its own design question, and modelling it here would answer a
    /// question this tool is not asking.
    pub pending_nanos: u64,
    /// The mean of its tail. Answers draw their own, so they complete out of the order they were asked
    /// for and the lane has to be put back together.
    pub pending_tail_nanos: u64,
    /// Commands a second it can answer. Zero means no limit, which is what `check` wants: its question
    /// is ordering, not capacity.
    pub pending_rate: u64,
    /// Holds the overlay may keep. What `check` varies, so eviction and pinning are exercised.
    pub resident_holds: usize,
    /// The engine's two memory windows, in blocks. `check` makes them small on purpose: with a real
    /// deployment's windows almost every resolution is answered from memory, so the fetch path — the
    /// candidate walk, the fingerprint confirmation, replies completing in the device's order rather
    /// than the lane's — would never run under fault injection, which is the one place it should.
    pub flush_blocks: usize,
    pub resident_blocks: usize,
    pub idem_nanos: u64,
    pub raft_nanos: u64,
    /// The mean of consensus's tail. A fixed round trip makes every batch equally late, which is the
    /// one thing a real quorum never is.
    pub raft_tail_nanos: u64,
    /// How long a day is on the virtual clock. Retention is the one thing in the engine measured in days,
    /// and a real day is many orders of magnitude past a two-thousand-step run — so the day is compressed
    /// here, which is the whole reason the engine is *told* the day instead of reading a clock. Zero means
    /// no day ever passes, which is what a capacity run wants: its question is not expiry.
    pub day_nanos: u64,
    /// Days a record lives: the promise plus the grace that keeps deletion from being early.
    pub lifetime_days: u64,
    /// Expiry voids offered per step, so a day's worth spreads out instead of arriving as one burst.
    pub expiry_blocks_per_round: usize,
}

/// What misbehaves, and by how much. Every one of these is off in `capacity`, whose question is what
/// the ledger does when nothing is broken.
#[derive(Debug, Clone, Copy, Default)]
pub struct Faults {
    /// Answer every nth lane reply early, breaking contract 1 on purpose.
    pub violate_order_every: u32,
    /// Answer every nth lookup as if the engine had applied less than it was given. The other way to
    /// break contract 1, and the only one that can be caught for a reply holding no place in its lane.
    pub stale_answer_every: u32,
    /// Refuse every nth batch.
    pub fail_every: u64,
    /// Answer every nth pair of batches in the wrong order.
    pub reorder_every: u64,
    /// Commands a component will hold in its inbox before it refuses. A real one has a bounded inbox,
    /// and refusing is what makes the sequencer defer a dispatch and pause intake.
    pub inbox_depth: usize,
    /// Stop reading acks every nth step, so the ack backlog fills and backpressure reaches the client.
    pub slow_client_every: u64,
    /// Slots the engine's index gets, zero for the default sizing. A handful means the declared maximum
    /// is passed within a run, which is a hold the log says exists and the store cannot take — the one
    /// thing the engine reports without being asked, and the seal that answers it. A fault rather than a
    /// timing, because the declaration being wrong is a misbehaviour, not a speed.
    pub index_slots: usize,
}

impl Timings {
    /// One seed, one set of component speeds, in steps rather than in wall time.
    pub fn draw(prng: &mut Prng, step_nanos: u64) -> Self {
        let mut pick = |n: u64| prng.next_u64() % n;
        Self {
            pending_nanos: pick(4) * step_nanos,
            pending_tail_nanos: pick(3) * step_nanos,
            pending_rate: 0,
            resident_holds: if pick(3) == 0 { 0 } else { 1 << 16 },
            // Windows small enough that records leave memory during a two-thousand-step run, so the seeds
            // cover the fetch path — the candidate walk, the fingerprint confirmation, and replies
            // completing in the device's order — while the faults are on. A deployment's windows would
            // answer everything from memory, which is the right answer there and no coverage here. One
            // seed in four keeps them wide, so the memory path is still exercised too.
            flush_blocks: if pick(4) == 0 {
                DEFAULT_FLUSH_BLOCKS
            } else {
                1 + pick(3) as usize
            },
            resident_blocks: if pick(4) == 0 {
                DEFAULT_RESIDENT_BLOCKS
            } else {
                pick(3) as usize
            },
            idem_nanos: pick(3) * step_nanos,
            raft_nanos: (1 + pick(4)) * step_nanos,
            raft_tail_nanos: pick(3) * step_nanos,
            // A day short enough that a run crosses several of them, so retention actually runs out and
            // the auto-void it produces is judged while the faults are on. One seed in four keeps the day
            // long enough never to pass, because a ledger nothing expires in is also a shape to cover.
            day_nanos: if pick(4) == 0 {
                0
            } else {
                (40 + pick(120)) * step_nanos
            },
            lifetime_days: 1 + pick(3),
            expiry_blocks_per_round: 1 + pick(8) as usize,
        }
    }
}

impl From<&crate::sim::Plan> for Timings {
    fn from(plan: &crate::sim::Plan) -> Self {
        Self {
            pending_nanos: plan.pending_nanos,
            pending_tail_nanos: plan.pending_tail_nanos,
            pending_rate: plan.pending_rate,
            flush_blocks: plan.flush_blocks,
            resident_blocks: plan.resident_blocks,
            // Eviction is not what a capacity run is asking about, so the overlay is given room and the
            // hit ratio is set outright.
            resident_holds: 1 << 20,
            idem_nanos: plan.idem_nanos,
            raft_nanos: plan.raft_nanos,
            raft_tail_nanos: plan.raft_tail_nanos,
            // Zero by default, so a capacity run asks what the ledger does against components of a given
            // speed and nothing else. Set them and the sweep competes with the traffic, which is the other
            // question: whether the throttle keeps up while clients are being served.
            day_nanos: plan.day_nanos,
            lifetime_days: plan.lifetime_days.max(1),
            expiry_blocks_per_round: plan.expiry_blocks_per_round,
        }
    }
}

impl Faults {
    /// One seed, one set of faults. Each is off in most seeds, so they are explored alone as well as
    /// together. A queue is always bounded — the depth is what varies, because a bound that is never
    /// reached explores no backpressure.
    pub fn draw(prng: &mut Prng) -> Self {
        let mut pick = |n: u64| prng.next_u64() % n;
        Self {
            violate_order_every: if pick(4) == 0 { 2 + pick(6) as u32 } else { 0 },
            stale_answer_every: if pick(5) == 0 { 2 + pick(8) as u32 } else { 0 },
            fail_every: if pick(3) == 0 { 3 + pick(8) } else { 0 },
            reorder_every: if pick(5) == 0 { 2 + pick(6) } else { 0 },
            inbox_depth: if pick(3) == 0 {
                2 + pick(8) as usize
            } else {
                64 + pick(256) as usize
            },
            slow_client_every: if pick(3) == 0 { 2 + pick(8) } else { 0 },
            // Some seeds are asked to outgrow their index, so the notice channel and the seal it causes
            // are explored alongside every other fault rather than only in a unit test. Rare, and roomy
            // enough that the seed runs a while first: a node that seals on its second hold spends the
            // rest of its steps sealed, which explores nothing else.
            index_slots: if pick(8) == 0 {
                64 + pick(192) as usize
            } else {
                0
            },
        }
    }

    /// Inboxes deep enough that a capacity run is not measuring a bound this tool chose.
    pub fn none(inbox_depth: usize) -> Self {
        Self {
            inbox_depth,
            ..Self::default()
        }
    }
}

/// The pending engine: the real overlay, the real lane ordering, a map for the store, and an inbox
/// read in the order it was written — which is what keeps a write ahead of a later lookup.
#[derive(Clone)]
pub struct PendingFake(Rc<RefCell<PendingState>>);

struct PendingState {
    overlay: HoldOverlay,
    /// Applies handed over, so a removal's marker can be stamped and retired the way the real port
    /// does it — this fake shares the engine, so it has to share the rule too.
    applies_sent: u64,
    /// The engine's own store, shared with the real one: a simulation that kept its own copy would
    /// be exercising something else.
    store: PendingEngine,
    inbox: VecDeque<(u64, PendingCommand)>,
    /// Each held result carries when the device finished it, so what the lane's order cost can be
    /// told apart from what the device cost.
    orderer: LaneOrderer<(u64, PendingReply), u64>,
    /// Claim to have applied less than it has, every nth answer.
    stale_answer_every: u32,
    answers: u64,
    /// Replies that kept no place in a lane: an exempt resolution's lookup. What says a sweep
    /// exercised the order exemption itself, not only the data check that covers it.
    exempt_replies: u64,
    /// What the engine has to say without being asked. Its own queue for the same reason the real one
    /// has: a notice answers no command, so it neither waits behind a reply nor delays one.
    notices: VecDeque<PendingNotice>,
    /// Reused by every sweep round, so expiry allocates nothing.
    expiring: Vec<Transfer>,
    expiries_offered: u64,
    ready: VecDeque<PendingReply>,
    now: u64,
    /// The engine, as the sequencer sees it: a service with a latency, a tail and a rate. Every
    /// resolution arrives here, because the record it judges by is the engine's; what the engine's own
    /// memory saves is IO below this, which is `ledgerfio`'s store model rather than one number here.
    engine: Server,
    prng: Prng,
    order_wait: FakeOrderWait,
    depth: usize,
    /// When this component next has anything to do. Kept rather than recomputed, because a capacity
    /// run takes a step every few hundred nanoseconds and a component answers every few
    /// microseconds: without it, every step would walk every lane.
    earliest: Option<u64>,
}

/// What putting a lane back in order cost. Separate from the device's own numbers, because a read
/// that finished in a millisecond and then waited nine for an earlier read on its lane is a speed
/// problem no per-read bound covers.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeOrderWait {
    pub released: u64,
    pub waited_nanos: u64,
    pub worst_nanos: u64,
    pub deepest: usize,
}

impl PendingFake {
    pub fn new(timings: Timings, faults: Faults, seed: u64) -> Self {
        Self(Rc::new(RefCell::new(PendingState {
            overlay: HoldOverlay::new(64, timings.resident_holds, 64),
            applies_sent: 0,
            store: PendingEngine::sized(
                if faults.index_slots > 0 {
                    faults.index_slots
                } else {
                    DEFAULT_SLOTS
                },
                timings.flush_blocks,
                timings.resident_blocks,
                Box::new(MemBlockStore::default()),
            ),
            inbox: VecDeque::new(),
            orderer: LaneOrderer::new(faults.violate_order_every),
            stale_answer_every: faults.stale_answer_every,
            answers: 0,
            exempt_replies: 0,
            notices: VecDeque::new(),
            expiring: Vec::new(),
            expiries_offered: 0,
            ready: VecDeque::new(),
            now: 0,
            engine: Server::new(
                timings.pending_nanos,
                timings.pending_tail_nanos,
                timings.pending_rate,
            ),
            prng: Prng::new(seed),
            order_wait: FakeOrderWait::default(),
            depth: faults.inbox_depth,
            earliest: None,
        })))
    }

    /// When this component next has something to hand back, so an idle clock can jump to it instead
    /// of crawling there.
    pub fn next_due(&self) -> Option<u64> {
        let state = self.0.borrow();
        if !state.ready.is_empty() {
            return Some(state.now);
        }
        state.earliest
    }

    /// Inserts the engine's index could not take. A hold the log says exists and the store does not have,
    /// which nothing inside the ledger can notice yet: a write has no reply to carry the news back on. So
    /// the simulation checks it from outside.
    pub fn overflowed(&self) -> u64 {
        self.0.borrow().store.traffic().overflowed
    }

    /// Reads the fake's engine had to take from its store. The number that says whether a sweep covered
    /// the fetch path at all — the candidate walk and the fingerprint confirmation only run there, and a
    /// run whose windows answered everything from memory has tested neither.
    pub fn store_reads(&self) -> u64 {
        self.0.borrow().store.traffic().store_reads
    }

    /// Replies that kept no place in a lane — the lookups of order-exempt resolutions.
    pub fn exempt_replies(&self) -> u64 {
        self.0.borrow().exempt_replies
    }

    /// Expiry voids the engine has offered. Here rather than on the virtual clock's own day count, because
    /// what matters is whether the sweep found anything for the sequencer to judge.
    pub fn expiries_offered(&self) -> u64 {
        self.0.borrow().expiries_offered
    }

    /// Expired days the sweep has not emptied yet. A level rather than a total, which is why a run reports
    /// the worst it reached and not a sum: what matters is whether the throttle ever fell further behind
    /// than the slack the index was sized with.
    pub fn days_behind(&self) -> u64 {
        self.0.borrow().store.days_behind()
    }

    /// Moves the engine's day on, which is the whole of what a clock does for retention. Driven by the
    /// simulation rather than read from a wall clock: expiry that could only be reached by waiting a day
    /// would be explored by no seed at all.
    pub fn open_day(&self, day: u64, lifetime_days: u64) {
        let mut state = self.0.borrow_mut();
        state.store.open_day(day, lifetime_days);
        // Something to do now, so an idle clock does not skip past the sweep.
        state.earliest = Some(state.earliest.map_or(state.now, |due| due.min(state.now)));
    }

    /// Offers the next slice of whatever ran out, as the voids that release it. Bounded per call for the
    /// same reason the real worker bounds it: a day's expiry must not arrive as one burst — in blocks of the
    /// expiring day, which is what bounds the work as well as the voids.
    pub fn sweep_expiry(&self, blocks_per_round: usize) {
        let mut state = self.0.borrow_mut();
        // The engine's own numbers, every round, the way `PendingWorker` checks them (rule 6). Asserted
        // here as well and not only there because this is the loop the fault seeds drive: the simulator
        // works the engine directly and never runs the worker, so an invariant living only in the worker is
        // one no seed would ever reach.
        debug_assert!(
            state.store.counts_agree(),
            "the index's per-segment counts no longer add up to its entries"
        );
        if !state.store.sweeping() || !state.notices.is_empty() {
            return;
        }
        let mut found = std::mem::take(&mut state.expiring);
        found.clear();
        state.store.expiring(blocks_per_round, &mut found);
        state.expiries_offered += found.len() as u64;
        for void in &found {
            state
                .notices
                .push_back(PendingNotice::HoldExpired { void: *void });
        }
        state.expiring = found;
    }

    pub fn engine(&self) -> ServerStats {
        self.0.borrow().engine.stats()
    }

    pub fn order_wait(&self) -> FakeOrderWait {
        self.0.borrow().order_wait
    }

    /// Forget what has been served, so a run that funds first and measures afterwards reports the
    /// measurement rather than the setup. The worst wait and the deepest lane need this rather than a
    /// subtraction: a maximum from before the measurement cannot be taken back out of one.
    pub fn reset_stats(&self) {
        let mut state = self.0.borrow_mut();
        state.engine.reset_stats();
        state.order_wait = FakeOrderWait::default();
    }

    pub fn drive(&self, now: u64) {
        let mut state = self.0.borrow_mut();
        state.now = now;
        if state.earliest.is_none_or(|due| due > now) {
            return;
        }
        while state.inbox.front().is_some_and(|(due, _)| *due <= now) {
            let (_, command) = state.inbox.pop_front().expect("a due command");
            match command {
                // A write occupies the engine like anything else. The store is updated now rather than at
                // completion: the inbox is in order, so a later lookup of the same hold is processed
                // after it either way, and the engine's queue is what the write actually costs.
                PendingCommand::Apply(effect) => {
                    state.engine_time();
                    if let Err(not_stored) = state.store.write(effect) {
                        state.notices.push_back(PendingNotice::HoldNotStored {
                            hold: not_stored.hold,
                        });
                    }
                }
                PendingCommand::Lookup(lookup) => {
                    let found = state.store.lookup(lookup.pending_ref);
                    let applied = state.claimed_applies();
                    let at = state.engine_time();

                    state.emit(
                        PendingReply {
                            correlation: lookup.correlation,
                            lane: lookup.lane,
                            seq: lookup.seq,
                            pending_ref: lookup.pending_ref,
                            found,
                            applied,
                        },
                        at,
                    );
                }
                // A fence reads nothing, but it is still a command the engine has to answer, and it
                // leaves in its lane's order — which is what makes it wait behind a read there.
                PendingCommand::Fence(fence) => {
                    let applied = state.claimed_applies();
                    let at = state.engine_time();
                    state.emit(
                        PendingReply {
                            correlation: fence.correlation,
                            lane: fence.lane,
                            seq: fence.seq,
                            pending_ref: TxId::ABSENT,
                            found: None,
                            applied,
                        },
                        at,
                    )
                }
            }
        }
        while let Some((finished, reply)) = state.orderer.pop_ready(now) {
            let waited = now.saturating_sub(finished);
            state.order_wait.released += 1;
            state.order_wait.waited_nanos += waited;
            state.order_wait.worst_nanos = state.order_wait.worst_nanos.max(waited);
            state.ready.push_back(reply);
        }
        let deepest = state.orderer.behind_heads();
        state.order_wait.deepest = state.order_wait.deepest.max(deepest);
        state.earliest = match (
            state.inbox.front().map(|(due, _)| *due),
            state.orderer.next_due(),
        ) {
            (Some(inbox), Some(held)) => Some(inbox.min(held)),
            (only, None) | (None, only) => only,
        };
    }
}

impl PendingState {
    /// A reply is finished at `at` — when it *leaves* is the orderer's business, which is the whole
    /// point: the lane's order is the component's work, and breaking it on purpose is a fault. An
    /// order-exempt reply keeps no place, so it leaves as soon as its own work is done.
    fn emit(&mut self, reply: PendingReply, at: u64) {
        if reply.seq == UNORDERED {
            self.exempt_replies += 1;
            self.orderer.push_unordered(at, (at, reply));
        } else {
            self.orderer.push(reply.lane, at, (at, reply));
        }
    }

    /// What this answer says the engine had applied. Truthful unless the fault is on.
    fn claimed_applies(&mut self) -> u64 {
        let applied = self.store.applied();
        self.answers += 1;
        let stale = self.stale_answer_every > 0
            && self
                .answers
                .is_multiple_of(u64::from(self.stale_answer_every));
        if stale {
            return applied.saturating_sub(1);
        }
        applied
    }

    /// What the engine's own work costs, whatever the command is.
    fn engine_time(&mut self) -> u64 {
        let now = self.now;
        self.engine.serve(now, &mut self.prng)
    }

    /// The overlay follows what the store is told, exactly as the real engine does.
    fn note(&mut self, command: PendingCommand) {
        let PendingCommand::Apply(effect) = command else {
            return;
        };
        match effect {
            PendingEffect::Create { tx_id, amount, .. } => self.overlay.created(tx_id, amount),
            PendingEffect::Reduce {
                pending_ref,
                remaining,
                ..
            } => self.overlay.note_remaining(pending_ref, remaining),
            PendingEffect::Remove { pending_ref, .. } => {
                self.applies_sent += 1;
                self.overlay.forget(pending_ref, self.applies_sent)
            }
        }
    }
}

impl PendingPort for PendingFake {
    fn send(&mut self, command: PendingCommand) -> Result<(), PendingCommand> {
        let mut state = self.0.borrow_mut();
        if state.inbox.len() >= state.depth {
            // A full queue is backpressure, not a lost command: the sequencer keeps it and retries.
            return Err(command);
        }
        state.note(command);
        let now = state.now;
        state.inbox.push_back((now, command));
        state.earliest = Some(state.earliest.map_or(now, |due| due.min(now)));
        Ok(())
    }

    fn poll(&self) -> Option<PendingReply> {
        self.0.borrow_mut().ready.pop_front()
    }

    fn notices(&self) -> Option<PendingNotice> {
        self.0.borrow_mut().notices.pop_front()
    }
}

impl PendingOverlay for PendingFake {
    fn hold_is_missing(&self, hold: TxId) -> bool {
        self.0.borrow().overlay.hold_is_missing(hold)
    }

    fn begin_lookup(&mut self, hold: TxId) {
        self.0.borrow_mut().overlay.begin_lookup(hold);
    }

    fn admit_lookup(&mut self, hold: TxId, remaining: Option<Amount>) {
        self.0.borrow_mut().overlay.admit_lookup(hold, remaining);
    }

    fn created(&mut self, hold: TxId, amount: Amount) {
        self.0.borrow_mut().overlay.created(hold, amount);
    }

    fn overlay(&self, hold: TxId) -> OverlayState {
        self.0.borrow().overlay.overlay(hold)
    }

    fn pin(&mut self, hold: TxId) {
        self.0.borrow_mut().overlay.pin(hold);
    }

    fn unpin(&mut self, hold: TxId) {
        self.0.borrow_mut().overlay.unpin(hold);
    }

    fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.0.borrow_mut().overlay.reserve(hold, amount, resolves);
    }

    fn release_reservation(&mut self, hold: TxId, amount: Amount) {
        self.0
            .borrow_mut()
            .overlay
            .release_reservation(hold, amount);
    }

    fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.0
            .borrow_mut()
            .overlay
            .compensate(hold, amount, resolves);
    }

    fn maintain(&mut self) -> usize {
        let mut state = self.0.borrow_mut();
        let applied = state.store.applied();
        state.overlay.maintain(applied)
    }

    fn overlay_len(&self) -> usize {
        self.0.borrow().overlay.len()
    }
}

/// Dedup with a virtual delay. Verdicts are independent of each other, but replies still leave in
/// each lane's order, like the real one.
#[derive(Clone)]
pub struct IdemFake(Rc<RefCell<IdemState>>);

struct IdemState {
    seen: FxHashMap<TxId, u64>,
    orderer: LaneOrderer<IdemReply, u64>,
    ready: VecDeque<IdemReply>,
    now: u64,
    delay: u64,
    depth: usize,
    outstanding: usize,
}

impl IdemFake {
    pub fn new(timings: Timings, faults: Faults) -> Self {
        Self(Rc::new(RefCell::new(IdemState {
            seen: FxHashMap::default(),
            orderer: LaneOrderer::new(0),
            ready: VecDeque::new(),
            now: 0,
            delay: timings.idem_nanos,
            depth: faults.inbox_depth,
            outstanding: 0,
        })))
    }

    pub fn next_due(&self) -> Option<u64> {
        let state = self.0.borrow();
        if !state.ready.is_empty() {
            return Some(state.now);
        }
        state.orderer.next_due()
    }

    pub fn drive(&self, now: u64) {
        let mut state = self.0.borrow_mut();
        state.now = now;
        while let Some(reply) = state.orderer.pop_ready(now) {
            state.outstanding = state.outstanding.saturating_sub(1);
            state.ready.push_back(reply);
        }
    }
}

impl IdempotencyPort for IdemFake {
    fn dispatch(&self, request: IdemRequest) -> Result<(), IdemRequest> {
        let mut state = self.0.borrow_mut();
        if state.outstanding >= state.depth {
            return Err(request);
        }
        state.outstanding += 1;
        // Same split as the real component: a `Serialize` ask takes the queue and leaves no record.
        let verdict = match request.ask {
            IdemAsk::Serialize => IdemVerdict::NotChecked,
            IdemAsk::Check => match state.seen.insert(request.tx_id, request.digest) {
                None => IdemVerdict::Fresh,
                Some(digest) if digest == request.digest => IdemVerdict::DuplicateSameBody,
                Some(_) => IdemVerdict::DuplicateDifferentBody,
            },
        };
        let due = state.now + state.delay;
        let reply = IdemReply {
            correlation: request.correlation,
            lane: request.lane,
            seq: request.seq,
            verdict,
        };
        // Same rule as the real stub: an order-exempt request's reply is not lane-ordered.
        if request.seq == UNORDERED {
            state.orderer.push_unordered(due, reply);
        } else {
            state.orderer.push(request.lane, due, reply);
        }
        Ok(())
    }

    fn poll(&self) -> Option<IdemReply> {
        self.0.borrow_mut().ready.pop_front()
    }
}

/// Consensus with a virtual round trip, which can refuse a batch or answer two of them in the wrong
/// order.
#[derive(Clone)]
pub struct RaftFake(Rc<RefCell<RaftState>>);

struct RaftState {
    inflight: VecDeque<(u64, RaftProposal, RaftOutcome)>,
    ready: VecDeque<RaftCommit>,
    now: u64,
    delay: u64,
    tail: u64,
    prng: Prng,
    seen: u64,
    fail_every: u64,
    reorder_every: u64,
}

impl RaftFake {
    pub fn new(timings: Timings, faults: Faults, seed: u64) -> Self {
        Self(Rc::new(RefCell::new(RaftState {
            inflight: VecDeque::new(),
            ready: VecDeque::new(),
            now: 0,
            delay: timings.raft_nanos,
            tail: timings.raft_tail_nanos,
            prng: Prng::new(seed),
            seen: 0,
            fail_every: faults.fail_every,
            reorder_every: faults.reorder_every,
        })))
    }

    pub fn next_due(&self) -> Option<u64> {
        let state = self.0.borrow();
        if !state.ready.is_empty() {
            return Some(state.now);
        }
        state.inflight.front().map(|(due, _, _)| *due)
    }

    pub fn drive(&self, now: u64) {
        let mut state = self.0.borrow_mut();
        state.now = now;
        while state
            .inflight
            .front()
            .is_some_and(|(due, _, _)| *due <= now)
        {
            let (_, proposal, outcome) = state.inflight.pop_front().expect("a due proposal");
            state.ready.push_back(RaftCommit {
                batch_id: proposal.batch_id,
                outcome,
                effects: proposal.effects,
            });
        }
    }
}

impl RaftPort for RaftFake {
    fn propose(&self, proposal: RaftProposal) -> Result<(), RaftProposal> {
        let mut state = self.0.borrow_mut();
        state.seen += 1;
        let outcome = if state.fail_every > 0 && state.seen.is_multiple_of(state.fail_every) {
            RaftOutcome::Failed
        } else {
            RaftOutcome::Committed
        };
        let tail = state.tail;
        // With a tail, a batch proposed later can draw an earlier answer. Consensus answers in commit
        // order, so a batch is never due before the one in front of it — which is head-of-line
        // blocking, and is the cost a fixed round trip hides.
        let ahead = state.inflight.back().map_or(0, |&(due, _, _)| due);
        let due = (state.now + state.delay + state.prng.exponential_nanos(tail)).max(ahead);
        let reorder = state.reorder_every > 0 && state.seen.is_multiple_of(state.reorder_every);
        state.inflight.push_back((due, proposal, outcome));
        if reorder && state.inflight.len() >= 2 {
            let last = state.inflight.len() - 1;
            state.inflight.swap(last - 1, last);
        }
        Ok(())
    }

    fn poll(&self) -> Option<RaftCommit> {
        self.0.borrow_mut().ready.pop_front()
    }
}
