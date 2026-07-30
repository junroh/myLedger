//! The three components, one thread and one clock short of the real ones. Each is a handle onto
//! shared state, so the simulation can drive it while the reactor owns a copy — the ports are traits
//! for exactly this reason, and nothing in the ledger changes to be simulated.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use ledger_base::ports::{
    HoldData, HoldView, IdemReply, IdemRequest, IdemVerdict, IdempotencyPort, OverlayState,
    PendingCommand, PendingEffect, PendingPort, PendingReply, RaftCommit, RaftOutcome, RaftPort,
    RaftProposal,
};
use ledger_base::{Amount, FxHashMap, Prng, TxId};
use ledger_pending::HoldOverlay;
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
    /// How often the pending engine can answer a resolution from memory, as a percentage. This is
    /// the black-box way to say it: how many entries a cache needs is that component's own question.
    pub pending_hit_percent: u64,
    pub idem_nanos: u64,
    pub raft_nanos: u64,
    /// The mean of consensus's tail. A fixed round trip makes every batch equally late, which is the
    /// one thing a real quorum never is.
    pub raft_tail_nanos: u64,
}

/// What misbehaves, and by how much. Every one of these is off in `capacity`, whose question is what
/// the ledger does when nothing is broken.
#[derive(Debug, Clone, Copy, Default)]
pub struct Faults {
    /// Answer every nth lane reply early, breaking contract 1 on purpose.
    pub violate_order_every: u32,
    /// Refuse every nth batch.
    pub fail_every: u64,
    /// Answer every nth pair of batches in the wrong order.
    pub reorder_every: u64,
    /// Commands a component will hold in its inbox before it refuses. A real one has a bounded inbox,
    /// and refusing is what makes the sequencer defer a dispatch and pause intake.
    pub inbox_depth: usize,
    /// Stop reading acks every nth step, so the ack backlog fills and backpressure reaches the client.
    pub slow_client_every: u64,
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
            // `check` is about the mechanism, so every hold is admitted and eviction decides the rest.
            pending_hit_percent: 100,
            idem_nanos: pick(3) * step_nanos,
            raft_nanos: (1 + pick(4)) * step_nanos,
            raft_tail_nanos: pick(3) * step_nanos,
        }
    }
}

impl From<&crate::sim::Plan> for Timings {
    fn from(plan: &crate::sim::Plan) -> Self {
        Self {
            pending_nanos: plan.pending_nanos,
            pending_tail_nanos: plan.pending_tail_nanos,
            pending_rate: plan.pending_rate,
            // Eviction is not what a capacity run is asking about, so the overlay is given room and the
            // hit ratio is set outright.
            resident_holds: 1 << 20,
            pending_hit_percent: plan.pending_hit_percent,
            idem_nanos: plan.idem_nanos,
            raft_nanos: plan.raft_nanos,
            raft_tail_nanos: plan.raft_tail_nanos,
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
            fail_every: if pick(3) == 0 { 3 + pick(8) } else { 0 },
            reorder_every: if pick(5) == 0 { 2 + pick(6) } else { 0 },
            inbox_depth: if pick(3) == 0 { 2 + pick(8) as usize } else { 64 + pick(256) as usize },
            slow_client_every: if pick(3) == 0 { 2 + pick(8) } else { 0 },
        }
    }

    /// Inboxes deep enough that a capacity run is not measuring a bound this tool chose.
    pub fn none(inbox_depth: usize) -> Self {
        Self { inbox_depth, ..Self::default() }
    }
}

/// The pending engine: the real overlay, the real lane ordering, a map for the store, and an inbox
/// read in the order it was written — which is what keeps a write ahead of a later lookup.
#[derive(Clone)]
pub struct PendingFake(Rc<RefCell<PendingState>>);

struct PendingState {
    overlay: HoldOverlay,
    store: FxHashMap<TxId, HoldData>,
    inbox: VecDeque<(u64, PendingCommand)>,
    /// Each held result carries when the device finished it, so what the lane's order cost can be
    /// told apart from what the device cost.
    orderer: LaneOrderer<(u64, PendingReply), u64>,
    ready: VecDeque<PendingReply>,
    now: u64,
    /// How often a resolution can be answered from memory. Applied when a hold is created, since that is
    /// where the engine would decide to keep it.
    hit_percent: u64,
    /// The engine, as the sequencer sees it: a service with a latency, a tail and a rate. A resident hit
    /// never reaches it — the sequencer reads the overlay inline — so what arrives here is misses,
    /// fences and writes.
    engine: Server,
    prng: Prng,
    order_wait: OrderWait,
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
pub struct OrderWait {
    pub released: u64,
    pub waited_nanos: u64,
    pub worst_nanos: u64,
    pub deepest: usize,
}

impl PendingFake {
    pub fn new(timings: Timings, faults: Faults, seed: u64) -> Self {
        Self(Rc::new(RefCell::new(PendingState {
            overlay: HoldOverlay::new(64, timings.resident_holds, 64),
            store: FxHashMap::default(),
            inbox: VecDeque::new(),
            orderer: LaneOrderer::new(faults.violate_order_every),
            ready: VecDeque::new(),
            now: 0,
            hit_percent: timings.pending_hit_percent,
            engine: Server::new(
                timings.pending_nanos,
                timings.pending_tail_nanos,
                timings.pending_rate,
            ),
            prng: Prng::new(seed),
            order_wait: OrderWait::default(),
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

    pub fn engine(&self) -> ServerStats {
        self.0.borrow().engine.stats()
    }

    pub fn order_wait(&self) -> OrderWait {
        self.0.borrow().order_wait
    }

    /// Forget what has been served, so a run that funds first and measures afterwards reports the
    /// measurement rather than the setup. The worst wait and the deepest lane need this rather than a
    /// subtraction: a maximum from before the measurement cannot be taken back out of one.
    pub fn reset_stats(&self) {
        let mut state = self.0.borrow_mut();
        state.engine.reset_stats();
        state.order_wait = OrderWait::default();
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
                    state.write(effect);
                }
                PendingCommand::Lookup(lookup) => {
                    let found = state.store.get(&lookup.pending_ref).copied();
                    let at = state.engine_time();

                    state.emit(
                        PendingReply {
                            correlation: lookup.correlation,
                            lane: lookup.lane,
                            seq: lookup.seq,
                            pending_ref: lookup.pending_ref,
                            found,
                        },
                        at,
                    );
                }
                // A fence reads nothing, but it is still a command the engine has to answer, and it
                // leaves in its lane's order — which is what makes it wait behind a read there.
                PendingCommand::Fence(fence) => {
                    let at = state.engine_time();
                    state.emit(
                        PendingReply {
                            correlation: fence.correlation,
                            lane: fence.lane,
                            seq: fence.seq,
                            pending_ref: TxId::ABSENT,
                            found: None,
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
        state.earliest = match (state.inbox.front().map(|(due, _)| *due), state.orderer.next_due()) {
            (Some(inbox), Some(held)) => Some(inbox.min(held)),
            (only, None) | (None, only) => only,
        };
    }
}

impl PendingState {
    /// A reply is finished at `at` — when it *leaves* is the orderer's business, which is the whole
    /// point: the lane's order is the component's work, and breaking it on purpose is a fault.
    fn emit(&mut self, reply: PendingReply, at: u64) {
        self.orderer.push(reply.lane, at, (at, reply));
    }

    /// Whether the engine keeps this hold in memory. Drawn, so the hit ratio is what was asked for
    /// rather than whatever an eviction policy happens to produce.
    fn keeps_it(&mut self) -> bool {
        self.hit_percent >= 100 || self.prng.next_u64() % 100 < self.hit_percent
    }

    /// What the engine's own work costs, whatever the command is.
    fn engine_time(&mut self) -> u64 {
        let now = self.now;
        self.engine.serve(now, &mut self.prng)
    }

    fn write(&mut self, effect: PendingEffect) {
        match effect {
            PendingEffect::Create {
                tx_id,
                debit_account,
                credit_account,
                amount,
                ledger,
                budget,
            } => {
                self.store.insert(
                    tx_id,
                    HoldData {
                        debit_account,
                        credit_account,
                        amount,
                        remaining: amount,
                        ledger,
                        budget,
                        budget_members: 0,
                        budget_remaining: 0,
                    },
                );
            }
            PendingEffect::Reduce { pending_ref, remaining } => {
                if let Some(hold) = self.store.get_mut(&pending_ref) {
                    hold.remaining = remaining;
                }
            }
            PendingEffect::Remove { pending_ref } => {
                self.store.remove(&pending_ref);
            }
        }
    }

    /// The overlay follows what the store is told, exactly as the real engine does.
    fn note(&mut self, command: PendingCommand) {
        let PendingCommand::Apply(effect) = command else {
            return;
        };
        match effect {
            PendingEffect::Create {
                tx_id,
                debit_account,
                credit_account,
                amount,
                ledger,
                budget,
            } if budget.is_absent() && self.keeps_it() => self.overlay.admit(
                tx_id,
                HoldData {
                    debit_account,
                    credit_account,
                    amount,
                    remaining: amount,
                    ledger,
                    budget,
                    budget_members: 0,
                    budget_remaining: 0,
                },
            ),
            PendingEffect::Reduce { pending_ref, remaining } => {
                self.overlay.note_remaining(pending_ref, remaining)
            }
            PendingEffect::Remove { pending_ref } => self.overlay.forget(pending_ref),
            _ => {}
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

    fn overlay_state(&self, hold: TxId) -> OverlayState {
        self.0.borrow().overlay.state(hold)
    }

    fn begin_lookup(&mut self, hold: TxId) {
        self.0.borrow_mut().overlay.begin_lookup(hold);
    }

    fn admit_lookup(&mut self, hold: TxId, found: Option<HoldData>) {
        self.0.borrow_mut().overlay.admit_lookup(hold, found);
    }

    fn view(&self, hold: TxId) -> Option<HoldView> {
        self.0.borrow().overlay.view(hold)
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
        self.0.borrow_mut().overlay.release_reservation(hold, amount);
    }

    fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.0.borrow_mut().overlay.compensate(hold, amount, resolves);
    }

    fn maintain(&mut self) -> usize {
        self.0.borrow_mut().overlay.maintain()
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
        let verdict = match state.seen.insert(request.tx_id, request.digest) {
            None => IdemVerdict::Fresh,
            Some(digest) if digest == request.digest => IdemVerdict::DuplicateSameBody,
            Some(_) => IdemVerdict::DuplicateDifferentBody,
        };
        let due = state.now + state.delay;
        state.orderer.push(
            request.lane,
            due,
            IdemReply {
                correlation: request.correlation,
                lane: request.lane,
                seq: request.seq,
                verdict,
            },
        );
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
        while state.inflight.front().is_some_and(|(due, _, _)| *due <= now) {
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
