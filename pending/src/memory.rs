use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ledger_base::ports::{
    HoldView, OverlayState, PendingCommand, PendingEffect, PendingPort, PendingReply,
};
use ledger_base::{
    Amount, Consumer, Footprint, FxHashMap, MapGauge, Prng, Producer, StagedProducer, TxId, channel,
};
use ledger_stubkit::{IdleBackoff, LaneOrderer, LatencyRange, WorkerThread};
use ledger_base::ports::HoldData;
use ledger_base::BudgetGroup;

use crate::overlay::HoldOverlay;

#[derive(Debug, Clone, Copy)]
pub struct MemoryPendingConfig {
    pub queue_capacity: usize,
    pub latency: LatencyRange,
    pub violate_order_every: u32,
    pub seed: u64,
    /// Overlay entries above which idle ones start being evicted. This is the engine's own cache
    /// policy, not the sequencer's.
    pub overlay_soft_limit: usize,
    /// Entries examined per housekeeping round, so a sweep never stalls the caller.
    pub eviction_per_round: usize,
}

impl Default for MemoryPendingConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 8192,
            latency: LatencyRange::new(Duration::from_micros(100), Duration::from_micros(800)),
            violate_order_every: 0,
            seed: 0x5eed_9e37,
            overlay_soft_limit: 1 << 20,
            eviction_per_round: 4096,
        }
    }
}

/// In-memory tier of the pending engine: it stores what the sequencer committed and
/// provides what a settle or void asks for, and judges nothing. The disk tier for holds
/// that outlive memory is not built yet.
pub struct MemoryPending {
    commands: Producer<PendingCommand>,
    results: Consumer<PendingReply>,
    /// Read inline, on the caller's own thread.
    overlay: HoldOverlay,
    /// What the store is holding, published by the worker because the store lives on its thread.
    store: Arc<Store>,
    _thread: WorkerThread,
}

/// The store's occupancy as the worker last published it.
#[derive(Debug, Default)]
struct Store {
    holds: MapGauge,
    budgets: MapGauge,
}

impl MemoryPending {
    pub fn start(config: MemoryPendingConfig) -> Self {
        let (commands, command_rx) = channel(config.queue_capacity);
        let (result_tx, results) = channel(config.queue_capacity);
        let store = Arc::new(Store::default());
        let worker_store = Arc::clone(&store);
        let thread = WorkerThread::spawn("pending", move |shutdown| {
            PendingWorker {
                commands: command_rx,
                results: StagedProducer::new(result_tx),
                holds: FxHashMap::default(),
                budgets: FxHashMap::default(),
                gauge: worker_store,
                orderer: LaneOrderer::new(config.violate_order_every),
                jitter: Prng::new(config.seed),
                latency: config.latency,
            }
            .run(shutdown)
        });
        Self {
            commands,
            results,
            overlay: HoldOverlay::new(
                config.queue_capacity,
                config.overlay_soft_limit,
                config.eviction_per_round,
            ),
            store,
            _thread: thread,
        }
    }

    /// What this engine is holding: the store on the worker's thread, and the overlay on the
    /// caller's. Both are memory — the disk tier for holds that outlive memory is not built, so
    /// nothing here is a disk figure.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        footprint.gauged_table::<TxId, HoldData>("engine holds", &self.store.holds);
        footprint.gauged_table::<BudgetGroup, BudgetState>(
            "engine budget groups",
            &self.store.budgets,
        );
        for part in self.overlay.footprint().parts() {
            footprint.other(
                part.name,
                part.entries,
                part.peak_entries,
                part.capacity,
                part.bytes,
            );
        }
        footprint
    }
}

impl PendingPort for MemoryPending {
    fn send(&mut self, command: PendingCommand) -> Result<(), PendingCommand> {
        self.commands.push(command)?;
        // The overlay follows what the store is told, so it never disagrees with it. A hold the
        // engine has just been told to create is known exactly here and now, which is why
        // resolving it later needs no lookup; a hold in a budget group is left out, because the
        // membership and the group's remainder are the store's to report, and a group needs both.
        match command {
            PendingCommand::Apply(PendingEffect::Create {
                tx_id,
                debit_account,
                credit_account,
                amount,
                ledger,
                budget,
            }) if budget.is_absent() => self.overlay.admit(
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
            PendingCommand::Apply(PendingEffect::Reduce { pending_ref, remaining }) => {
                self.overlay.note_remaining(pending_ref, remaining)
            }
            PendingCommand::Apply(PendingEffect::Remove { pending_ref }) => {
                self.overlay.forget(pending_ref)
            }
            _ => {}
        }
        Ok(())
    }

    fn poll(&self) -> Option<PendingReply> {
        self.results.pop()
    }

    fn overlay_state(&self, hold: TxId) -> OverlayState {
        self.overlay.state(hold)
    }

    fn begin_lookup(&mut self, hold: TxId) {
        self.overlay.begin_lookup(hold);
    }

    fn admit_lookup(&mut self, hold: TxId, found: Option<HoldData>) {
        self.overlay.admit_lookup(hold, found);
    }

    fn view(&self, hold: TxId) -> Option<HoldView> {
        self.overlay.view(hold)
    }

    fn pin(&mut self, hold: TxId) {
        self.overlay.pin(hold);
    }

    fn unpin(&mut self, hold: TxId) {
        self.overlay.unpin(hold);
    }

    fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.overlay.reserve(hold, amount, resolves);
    }

    fn release_reservation(&mut self, hold: TxId, amount: Amount) {
        self.overlay.release_reservation(hold, amount);
    }

    fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.overlay.compensate(hold, amount, resolves);
    }

    fn maintain(&mut self) -> usize {
        self.overlay.maintain()
    }

    fn overlay_len(&self) -> usize {
        self.overlay.len()
    }
}

/// The group as the store knows it. Membership is why the sequencer can tell a partial
/// resolution from a complete one.
#[derive(Debug, Clone, Copy, Default)]
struct BudgetState {
    members: u32,
    remaining: Amount,
}

struct PendingWorker {
    commands: Consumer<PendingCommand>,
    results: StagedProducer<PendingReply>,
    holds: FxHashMap<TxId, HoldData>,
    budgets: FxHashMap<BudgetGroup, BudgetState>,
    gauge: Arc<Store>,
    orderer: LaneOrderer<PendingReply>,
    jitter: Prng,
    latency: LatencyRange,
}

impl PendingWorker {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut backoff = IdleBackoff::new();
        while !shutdown.load(Ordering::Relaxed) {
            let progress = self.drain_commands() | self.deliver();
            backoff.record(progress);
        }
    }

    /// Once per round rather than once per command: the store's size is asked for by a report at the
    /// end of a run, and paying six atomic stores per write would be a cost per request for it.
    fn publish(&self) {
        self.gauge.holds.publish(self.holds.len(), self.holds.capacity());
        self.gauge.budgets.publish(self.budgets.len(), self.budgets.capacity());
    }

    fn drain_commands(&mut self) -> bool {
        let mut progress = false;
        while let Some(command) = self.commands.pop() {
            progress = true;
            match command {
                // Read at dequeue time, deliver later: the answer must reflect the store as
                // of this lookup's place in the command stream, not as of its delivery.
                PendingCommand::Lookup(lookup) => {
                    let result = PendingReply {
                        correlation: lookup.correlation,
                        lane: lookup.lane,
                        seq: lookup.seq,
                        pending_ref: lookup.pending_ref,
                        found: self.hold_with_group(lookup.pending_ref),
                    };
                    let due = self.latency.due_from(Instant::now(), &mut self.jitter);
                    self.orderer.push(lookup.lane, due, result);
                }
                // A fence reads nothing; the lane orderer alone gives it its meaning.
                PendingCommand::Fence(fence) => {
                    let result = PendingReply {
                        correlation: fence.correlation,
                        lane: fence.lane,
                        seq: fence.seq,
                        pending_ref: TxId::ABSENT,
                        found: None,
                    };
                    self.orderer.push(fence.lane, Instant::now(), result);
                }
                PendingCommand::Apply(effect) => self.write(effect),
            }
        }
        if progress {
            self.publish();
        }
        progress
    }

    fn deliver(&mut self) -> bool {
        if !self.results.flush() {
            return false;
        }
        let now = Instant::now();
        let mut progress = false;
        while !self.results.is_stuck() {
            match self.orderer.pop_ready(now) {
                Some(result) => {
                    self.results.send(result);
                    progress = true;
                }
                None => break,
            }
        }
        progress
    }

    /// Answers with the hold and, when it belongs to one, its group as a whole.
    fn hold_with_group(&self, pending_ref: TxId) -> Option<HoldData> {
        let mut hold = *self.holds.get(&pending_ref)?;
        if let Some(state) = self.budgets.get(&hold.budget) {
            hold.budget_members = state.members;
            hold.budget_remaining = state.remaining;
        }
        Some(hold)
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
                self.holds.insert(
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
                if !budget.is_absent() {
                    let state = self.budgets.entry(budget).or_default();
                    state.members += 1;
                    state.remaining += amount;
                }
            }
            PendingEffect::Reduce { pending_ref, remaining } => {
                let Some(hold) = self.holds.get_mut(&pending_ref) else {
                    return;
                };
                let consumed = hold.remaining - remaining;
                hold.remaining = remaining;
                let budget = hold.budget;
                if let Some(state) = self.budgets.get_mut(&budget) {
                    state.remaining -= consumed;
                }
            }
            PendingEffect::Remove { pending_ref } => {
                let Some(hold) = self.holds.remove(&pending_ref) else {
                    return;
                };
                if hold.budget.is_absent() {
                    return;
                }
                let Some(state) = self.budgets.get_mut(&hold.budget) else {
                    return;
                };
                state.members = state.members.saturating_sub(1);
                state.remaining -= hold.remaining;
                if state.members == 0 {
                    self.budgets.remove(&hold.budget);
                }
            }
        }
    }
}
