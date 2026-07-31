use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ledger_base::ports::HoldData;
use ledger_base::ports::{
    OverlayState, PendingCommand, PendingEffect, PendingLookup, PendingOverlay, PendingPort,
    PendingReply,
};
use ledger_base::BudgetGroup;
use ledger_base::{
    channel, Amount, Consumer, Footprint, FxHashMap, LedgerError, MapGauge, Prng, Producer,
    StagedProducer, TxId,
};
use ledger_stubkit::{IdleBackoff, LatencyRange, WorkerThread};

use crate::block::{
    BlockAddr, BlockStore, LatencyBlockStore, LogTraffic, MemBlockStore, BLOCK_BYTES,
    RECORDS_PER_BLOCK,
};
use crate::engine::{BudgetState, PendingEngine, Started};
use crate::index::{LOAD_TARGET, SLOT_BYTES};
use crate::orderer::OrderWait;
use crate::orderer::Orderer;
use crate::overlay::HoldOverlay;

#[derive(Debug, Clone, Copy)]
pub struct MemoryPendingConfig {
    pub queue_capacity: usize,
    /// A delay injected into every reply, so a test that needs replies to queue up has something for
    /// `violate_order_every` to reorder. **Not** a model of the engine: the index, the buffer and the
    /// block store do their own work now, and adding an invented delay on top of it would count the
    /// same time twice. Zero unless a test asks.
    pub latency: LatencyRange,
    pub violate_order_every: u32,
    /// Answer every nth lookup as if the engine had not yet applied what it was given. The other way to
    /// break contract 1, and the one the lane's order cannot catch: a request that keeps no place in its
    /// lane has only the data check left. A fault, and the only reason this knob exists.
    pub stale_answer_every: u32,
    pub seed: u64,
    /// Overlay entries above which idle ones start being evicted. This is the engine's own cache
    /// policy, not the sequencer's.
    pub overlay_soft_limit: usize,
    /// Entries examined per housekeeping round, so a sweep never stalls the caller.
    pub eviction_per_round: usize,
    /// Blocks of records the engine keeps before compacting the oldest out. A count rather than a
    /// duration: the engine has no clock, and what a window costs is bytes either way.
    /// What the deployment says the worst case is. The index is sized from it and never grows, so this is
    /// the one place the size is decided.
    pub capacity: PendingCapacity,
    /// What answers for blocks. Memory that adds no latency by default; a device's timing is asked for.
    pub store: StoreModel,
    /// The most the index may take. A configuration whose declared worst case needs more than this is
    /// refused at the start rather than discovered as an allocation nobody planned.
    pub index_budget_bytes: usize,
}

/// How the block store behaves. Zero base latency is the exact store — memory, no delay — which is what
/// every other answer is measured against. Anything else wraps it in a device's timing, because there is
/// no disk under this yet and pretending otherwise in silence would be worse than saying so.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreModel {
    pub read_base_nanos: u64,
    /// The mean of the tail. A fixed latency completes every read in the order it was asked for and hides
    /// what putting a lane back in order costs.
    pub read_tail_nanos: u64,
    /// Reads a second the device can serve, zero for no ceiling.
    pub iops: u64,
    /// Reads it will hold at once. Past this the engine keeps the command and asks again.
    pub queue_depth: usize,
}

impl StoreModel {
    fn build(&self, seed: u64) -> Box<dyn BlockStore> {
        let exact = Box::new(MemBlockStore::default());
        if self.read_base_nanos == 0 && self.read_tail_nanos == 0 && self.iops == 0 {
            return exact;
        }
        Box::new(LatencyBlockStore::new(
            exact,
            self.read_base_nanos,
            self.read_tail_nanos,
            self.iops,
            self.queue_depth.max(1),
            seed,
        ))
    }
}

/// The worst case the business declares, from which every size in the engine follows. Inputs rather
/// than sizes: a block count or a slot count configured beside these could disagree with them, and then
/// the sizing rule would live in two places.
#[derive(Debug, Clone, Copy)]
pub struct PendingCapacity {
    pub daily_arrivals: u64,
    /// The share of a day's transfers that are still unresolved when the retention window ends — the
    /// worst case, not the expected one.
    pub worst_survivor_share: f64,
    pub retention_days: u64,
    /// The share of a day's transfers still unresolved when their block reaches the end of the flush
    /// window. Larger than `worst_survivor_share`, because a hold alive after thirty-two days was alive
    /// after an hour. A run measures the same quantity as `died in buffer`, so the declared value is
    /// checkable rather than trusted.
    pub survives_flush_window: f64,
    /// How long a record may go unwritten. This is a recovery bound, not a latency one: what has not
    /// reached the store exists only in memory and has to be in the checkpoint, so a day here would make
    /// recovery too slow whatever it saved in writes.
    pub flush_window_hours: u64,
    /// How long a record stays readable in memory after it has been written. This is the latency bound:
    /// resolutions that happen within it cost no IO. Independent of the flush window in both directions.
    pub residency_hours: u64,
}

impl Default for PendingCapacity {
    /// Small enough for a test or a local run to reach, and stated rather than implied: a default that
    /// hid the sizing rule would be the thing this configuration exists to prevent. Stated here alone, so
    /// a tool's own defaults can be these rather than a second copy of them.
    fn default() -> Self {
        Self {
            daily_arrivals: 1_000_000,
            worst_survivor_share: 0.5,
            retention_days: 2,
            survives_flush_window: 0.5,
            flush_window_hours: 1,
            residency_hours: 24,
        }
    }
}

impl PendingCapacity {
    /// Holds alive at once in the worst case the configuration declares.
    pub fn declared_maximum(&self) -> u64 {
        (self.daily_arrivals as f64 * self.worst_survivor_share) as u64 * self.retention_days
    }

    /// Slots the index needs to hold that maximum at the load factor the cascade was measured against.
    /// Rounded up by `HoldTable` to a power of two of buckets, so the answer here is a floor.
    pub fn slots(&self) -> usize {
        (self.declared_maximum() as f64 / LOAD_TARGET).ceil() as usize
    }

    /// Blocks the writeback buffer needs: the arrivals of one flush window, in full. Nothing is
    /// compacted out of it yet, so the survivor share does not come into this one.
    pub fn flush_blocks(&self) -> usize {
        Self::blocks_for(self.arrivals_per_hour() * self.flush_window_hours)
    }

    /// Blocks residency needs: the survivors of one residency window. Only survivors, because what is
    /// resident has already been compacted — which is the difference between a few gigabytes and a day
    /// of arrivals, and the reason a day is affordable at all.
    pub fn resident_blocks(&self) -> usize {
        let arrivals = self.arrivals_per_hour() * self.residency_hours;
        Self::blocks_for((arrivals as f64 * self.survives_flush_window) as u64)
    }

    fn arrivals_per_hour(&self) -> u64 {
        self.daily_arrivals / 24
    }

    fn blocks_for(records: u64) -> usize {
        (records as usize).div_ceil(RECORDS_PER_BLOCK).max(1)
    }
}

impl MemoryPendingConfig {
    /// Refuses combinations that would misbehave silently — the same job `ReactorConfig::validate` does.
    pub fn validate(&self) -> Result<(), LedgerError> {
        let capacity = &self.capacity;
        let sane = self.queue_capacity > 0
            && self.eviction_per_round > 0
            && capacity.daily_arrivals >= 24
            && capacity.retention_days > 0
            && capacity.worst_survivor_share > 0.0
            && capacity.worst_survivor_share <= 1.0
            // A hold alive when retention ends was alive an hour in, so a configuration that declares
            // otherwise is not describing a workload that exists.
            && capacity.survives_flush_window >= capacity.worst_survivor_share
            && capacity.survives_flush_window <= 1.0
            && capacity.flush_window_hours > 0
            // Residency shorter than the flush window would mean records leaving memory before they are
            // written, which is not a window at all.
            && capacity.residency_hours >= capacity.flush_window_hours
            && capacity.slots() * SLOT_BYTES <= self.index_budget_bytes;
        if sane {
            Ok(())
        } else {
            Err(LedgerError::ConfigInvalid)
        }
    }
}

impl Default for MemoryPendingConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 8192,
            latency: LatencyRange::fixed(Duration::ZERO),
            violate_order_every: 0,
            stale_answer_every: 0,
            seed: 0x5eed_9e37,
            overlay_soft_limit: 1 << 20,
            eviction_per_round: 4096,
            capacity: PendingCapacity::default(),
            index_budget_bytes: 1 << 30,
            store: StoreModel {
                queue_depth: 128,
                ..StoreModel::default()
            },
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
    occupancy: Arc<Occupancy>,
    _thread: WorkerThread,
}

/// What the log has done, published by the worker for whoever reports on the run. Counters rather
/// than sizes, which is why they are not part of the footprint: where a read went and whether a record
/// ever reached the store are rates, and the second is the figure the design's own inputs disagree
/// about.
#[derive(Debug, Default)]
struct TrafficGauge {
    appended: AtomicU64,
    died_in_buffer: AtomicU64,
    flushed: AtomicU64,
    left_memory: AtomicU64,
    buffer_reads: AtomicU64,
    resident_reads: AtomicU64,
    store_reads: AtomicU64,
    apply_store_reads: AtomicU64,
    inflight_peak: AtomicU64,
    index_live: AtomicU64,
    index_slots: AtomicU64,
    worst_cascade: AtomicU64,
    ambiguous: AtomicU64,
    overflowed: AtomicU64,
}

impl TrafficGauge {
    fn publish(&self, traffic: LogTraffic) {
        self.appended.store(traffic.appended, Ordering::Relaxed);
        self.died_in_buffer
            .store(traffic.died_in_buffer, Ordering::Relaxed);
        self.flushed.store(traffic.flushed, Ordering::Relaxed);
        self.left_memory
            .store(traffic.left_memory, Ordering::Relaxed);
        self.buffer_reads
            .store(traffic.buffer_reads, Ordering::Relaxed);
        self.resident_reads
            .store(traffic.resident_reads, Ordering::Relaxed);
        self.store_reads
            .store(traffic.store_reads, Ordering::Relaxed);
        self.apply_store_reads
            .store(traffic.apply_store_reads, Ordering::Relaxed);
        self.inflight_peak
            .store(traffic.inflight_peak as u64, Ordering::Relaxed);
        self.index_live
            .store(traffic.index_live as u64, Ordering::Relaxed);
        self.index_slots
            .store(traffic.index_slots as u64, Ordering::Relaxed);
        self.worst_cascade
            .store(u64::from(traffic.worst_cascade), Ordering::Relaxed);
        self.ambiguous.store(traffic.ambiguous, Ordering::Relaxed);
        self.overflowed.store(traffic.overflowed, Ordering::Relaxed);
    }

    fn read(&self) -> LogTraffic {
        LogTraffic {
            appended: self.appended.load(Ordering::Relaxed),
            died_in_buffer: self.died_in_buffer.load(Ordering::Relaxed),
            flushed: self.flushed.load(Ordering::Relaxed),
            left_memory: self.left_memory.load(Ordering::Relaxed),
            buffer_reads: self.buffer_reads.load(Ordering::Relaxed),
            resident_reads: self.resident_reads.load(Ordering::Relaxed),
            store_reads: self.store_reads.load(Ordering::Relaxed),
            apply_store_reads: self.apply_store_reads.load(Ordering::Relaxed),
            inflight_peak: self.inflight_peak.load(Ordering::Relaxed) as usize,
            index_live: self.index_live.load(Ordering::Relaxed) as usize,
            index_slots: self.index_slots.load(Ordering::Relaxed) as usize,
            worst_cascade: self.worst_cascade.load(Ordering::Relaxed) as u32,
            ambiguous: self.ambiguous.load(Ordering::Relaxed),
            overflowed: self.overflowed.load(Ordering::Relaxed),
        }
    }
}

/// The store's occupancy as the worker last published it.
#[derive(Debug, Default)]
struct Occupancy {
    holds: MapGauge,
    budgets: MapGauge,
    /// Blocks in the store, which is where a record ends up once its buffered block is compacted.
    blocks: MapGauge,
    /// Blocks in the writeback buffer, against the flush window it was given.
    buffer: MapGauge,
    /// Blocks written to the store and kept in memory anyway, against the residency window.
    resident: MapGauge,
    traffic: TrafficGauge,
    /// What putting each lane back in order cost. Published because it is the one cost no per-read bound
    /// covers, and until now the engine computed it and nobody could read it.
    order_wait: OrderWaitGauge,
}

/// The orderer's four numbers across the thread boundary. A gauge rather than part of `LogTraffic`
/// because it is the orderer's, not the log's: a read that finished on time and then waited for an
/// earlier read on its lane is a different fact from where that read was answered.
#[derive(Debug, Default)]
struct OrderWaitGauge {
    released: AtomicU64,
    held_for_order: AtomicU64,
    order_nanos: AtomicU64,
    order_worst_nanos: AtomicU64,
    delivery_nanos: AtomicU64,
    deepest_lane: AtomicU64,
}

impl OrderWaitGauge {
    fn publish(&self, wait: OrderWait) {
        self.released.store(wait.released, Ordering::Relaxed);
        self.held_for_order
            .store(wait.held_for_order, Ordering::Relaxed);
        self.order_nanos.store(wait.order_nanos, Ordering::Relaxed);
        self.order_worst_nanos
            .store(wait.order_worst_nanos, Ordering::Relaxed);
        self.delivery_nanos
            .store(wait.delivery_nanos, Ordering::Relaxed);
        self.deepest_lane
            .store(wait.deepest_lane as u64, Ordering::Relaxed);
    }

    fn read(&self) -> OrderWait {
        OrderWait {
            released: self.released.load(Ordering::Relaxed),
            held_for_order: self.held_for_order.load(Ordering::Relaxed),
            order_nanos: self.order_nanos.load(Ordering::Relaxed),
            order_worst_nanos: self.order_worst_nanos.load(Ordering::Relaxed),
            delivery_nanos: self.delivery_nanos.load(Ordering::Relaxed),
            deepest_lane: self.deepest_lane.load(Ordering::Relaxed) as usize,
        }
    }
}

impl MemoryPending {
    /// Refuses a configuration that would misbehave silently before spawning anything, the same way
    /// `Reactor::new` does — the sizes here are derived from declared inputs, so an incoherent
    /// declaration has to be an error at the start rather than a window nobody meant.
    pub fn start(config: MemoryPendingConfig) -> Result<Self, LedgerError> {
        config.validate()?;
        let (commands, command_rx) = channel(config.queue_capacity);
        let (result_tx, results) = channel(config.queue_capacity);
        let occupancy = Arc::new(Occupancy::default());
        let worker_occupancy = Arc::clone(&occupancy);
        let thread = WorkerThread::spawn("pending", move |shutdown| {
            PendingWorker {
                commands: command_rx,
                results: StagedProducer::new(result_tx),
                store: PendingEngine::sized(
                    config.capacity.slots(),
                    config.capacity.flush_blocks(),
                    config.capacity.resident_blocks(),
                    config.store.build(config.seed ^ 0xb10c),
                ),
                occupancy: worker_occupancy,
                orderer: Orderer::new(config.violate_order_every),
                stale_answer_every: config.stale_answer_every,
                answers: 0,
                inflight: FxHashMap::default(),
                handles: 0,
                deferred: None,
                jitter: Prng::new(config.seed),
                latency: config.latency,
                started: Instant::now(),
            }
            .run(shutdown)
        });
        Ok(Self {
            commands,
            results,
            overlay: HoldOverlay::new(
                config.queue_capacity,
                config.overlay_soft_limit,
                config.eviction_per_round,
            ),
            occupancy,
            _thread: thread,
        })
    }

    /// Where the reads went, and how much of what was written never had to be written out.
    pub fn traffic(&self) -> LogTraffic {
        self.occupancy.traffic.read()
    }

    /// What keeping each lane in seq order cost on top of the reads themselves.
    pub fn order_wait(&self) -> OrderWait {
        self.occupancy.order_wait.read()
    }

    /// What this engine is holding: the store on the worker's thread, and the overlay on the
    /// caller's. Both are memory — the disk tier for holds that outlive memory is not built, so
    /// nothing here is a disk figure.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        // The index is what a hold costs in memory; the record itself lives on a block, and the blocks
        // are where the store's own size is. Both are memory today — the disk tier is not built — so
        // neither figure is a disk figure.
        footprint.gauged_table::<TxId, BlockAddr>("engine index", &self.occupancy.holds);
        footprint.gauged_table::<BudgetGroup, BudgetState>(
            "engine budget groups",
            &self.occupancy.budgets,
        );
        // Three figures for one set of records, because they answer three different questions: what a
        // checkpoint would have to carry (unwritten), what memory costs to keep resolutions off the disk
        // (resident), and what the store holds — the only one that would not be memory once the store is
        // a disk.
        let buffered = self.occupancy.buffer.entries();
        footprint.other(
            "engine writeback buffer",
            buffered,
            self.occupancy.buffer.peak(),
            self.occupancy.buffer.capacity(),
            buffered * BLOCK_BYTES,
        );
        let resident = self.occupancy.resident.entries();
        footprint.other(
            "engine resident blocks",
            resident,
            self.occupancy.resident.peak(),
            self.occupancy.resident.capacity(),
            resident * BLOCK_BYTES,
        );
        let blocks = self.occupancy.blocks.entries();
        footprint.other(
            "engine record blocks",
            blocks,
            self.occupancy.blocks.peak(),
            0,
            blocks * BLOCK_BYTES,
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
        // What a hold has left follows the write the engine is sent, and nothing else writes it. That
        // is the whole of what the overlay keeps: the record itself is the engine's, and a lookup is
        // how a request gets one.
        match command {
            PendingCommand::Apply(PendingEffect::Create { tx_id, amount, .. }) => {
                self.overlay.created(tx_id, amount)
            }
            PendingCommand::Apply(PendingEffect::Reduce {
                pending_ref,
                remaining,
                ..
            }) => self.overlay.note_remaining(pending_ref, remaining),
            PendingCommand::Apply(PendingEffect::Remove { pending_ref, .. }) => {
                self.overlay.forget(pending_ref)
            }
            _ => {}
        }
        Ok(())
    }

    fn poll(&self) -> Option<PendingReply> {
        self.results.pop()
    }
}

impl PendingOverlay for MemoryPending {
    fn hold_is_missing(&self, hold: TxId) -> bool {
        self.overlay.hold_is_missing(hold)
    }

    fn begin_lookup(&mut self, hold: TxId) {
        self.overlay.begin_lookup(hold);
    }

    fn admit_lookup(&mut self, hold: TxId, remaining: Option<Amount>) {
        self.overlay.admit_lookup(hold, remaining);
    }

    fn created(&mut self, hold: TxId, amount: Amount) {
        self.overlay.created(hold, amount);
    }

    fn overlay(&self, hold: TxId) -> OverlayState {
        self.overlay.overlay(hold)
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

/// The group as the store knows it lives with the store — see `engine.rs`.
struct PendingWorker {
    commands: Consumer<PendingCommand>,
    results: StagedProducer<PendingReply>,
    store: PendingEngine,
    occupancy: Arc<Occupancy>,
    orderer: Orderer<PendingReply>,
    /// Claim to have applied less than it has, every nth answer. A fault: the data check on the reply is
    /// what has to catch it, because a request keeping no place in its lane has no order to be checked.
    stale_answer_every: u32,
    answers: u64,
    /// Lookups the store is fetching for, by the handle the engine was given. The engine carries the
    /// walk; this carries who asked.
    inflight: FxHashMap<u64, PendingLookup>,
    handles: u64,
    /// A command the store would not take yet. Kept rather than dropped, and retried before anything
    /// else is dequeued, because the order places are reserved in is the lane's order.
    deferred: Option<PendingCommand>,
    jitter: Prng,
    latency: LatencyRange,
    /// Nanoseconds since the worker started. The orderer and the device model both work in a plain
    /// integer clock, so the origin is the caller's to choose — here it is the thread's own start, and in
    /// a simulation it is the virtual clock.
    started: Instant,
}

impl PendingWorker {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut backoff = IdleBackoff::new();
        while !shutdown.load(Ordering::Relaxed) {
            let progress = self.drain_commands() | self.harvest() | self.deliver();
            backoff.record(progress);
        }
    }

    /// Once per round rather than once per command: the store's size is asked for by a report at the
    /// end of a run, and paying six atomic stores per write would be a cost per request for it.
    fn publish(&self) {
        self.store.publish(
            &self.occupancy.holds,
            &self.occupancy.budgets,
            &self.occupancy.blocks,
            &self.occupancy.buffer,
            &self.occupancy.resident,
        );
        self.occupancy.traffic.publish(self.store.traffic());
        self.occupancy.order_wait.publish(self.orderer.order_wait());
    }

    fn drain_commands(&mut self) -> bool {
        let mut progress = false;
        if let Some(held) = self.deferred.take() {
            if !self.take(held) {
                return false;
            }
            progress = true;
        }
        while let Some(command) = self.commands.pop() {
            progress = true;
            if !self.take(command) {
                break;
            }
        }
        if progress {
            self.publish();
        }
        progress
    }

    /// False when the store would not take the read this command needs. The command is kept and the
    /// round ends: dequeuing the next one would reserve its place out of order.
    fn take(&mut self, command: PendingCommand) -> bool {
        {
            match command {
                // Read at dequeue time, deliver later: the answer must reflect the store as
                // of this lookup's place in the command stream, not as of its delivery.
                // The place is reserved here, in the order the commands arrived. What fills it may
                // arrive much later, from the store.
                PendingCommand::Lookup(lookup) => {
                    let now = self.now();
                    self.handles += 1;
                    let handle = self.handles;
                    match self.store.begin_lookup(handle, lookup.pending_ref, now) {
                        Started::Busy => {
                            self.deferred = Some(command);
                            return false;
                        }
                        Started::Answered(found) => {
                            self.orderer.expect(lookup.lane, lookup.seq);
                            let ready_at = now + self.injected_delay();
                            let reply = self.reply(&lookup, found);
                            self.orderer.fill(lookup.lane, lookup.seq, ready_at, reply);
                        }
                        Started::Fetching => {
                            self.orderer.expect(lookup.lane, lookup.seq);
                            self.inflight.insert(handle, lookup);
                        }
                    }
                }
                // A fence reads nothing; the order it leaves in is all it is for.
                PendingCommand::Fence(fence) => {
                    let result = PendingReply {
                        correlation: fence.correlation,
                        lane: fence.lane,
                        seq: fence.seq,
                        pending_ref: TxId::ABSENT,
                        found: None,
                        applied: self.claimed_applies(),
                    };
                    let ready_at = self.now() + self.injected_delay();
                    self.orderer.expect(fence.lane, fence.seq);
                    self.orderer.fill(fence.lane, fence.seq, ready_at, result);
                }
                PendingCommand::Apply(effect) => self.store.write(effect),
            }
        }
        true
    }

    fn reply(&mut self, lookup: &PendingLookup, found: Option<HoldData>) -> PendingReply {
        PendingReply {
            correlation: lookup.correlation,
            lane: lookup.lane,
            seq: lookup.seq,
            pending_ref: lookup.pending_ref,
            found,
            applied: self.claimed_applies(),
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
            // One decision short of the truth is enough: the check is `at least what I was given`.
            return applied.saturating_sub(1);
        }
        applied
    }

    /// Fills the places of lookups the store has answered. Their completions arrive in the device's
    /// order; the orderer is what turns that back into the lane's.
    fn harvest(&mut self) -> bool {
        let now = self.now();
        let mut progress = false;
        while let Some((handle, found)) = self.store.harvest(now) {
            let Some(lookup) = self.inflight.remove(&handle) else {
                continue;
            };
            let ready_at = now + self.injected_delay();
            let reply = self.reply(&lookup, found);
            self.orderer.fill(lookup.lane, lookup.seq, ready_at, reply);
            progress = true;
        }
        progress
    }

    /// Nanoseconds since this worker started. An integer clock, because both the orderer and the device
    /// model take one and the origin belongs to whoever owns the loop.
    fn now(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }

    /// A delay asked for by a test that needs replies to queue up so the fault has something to
    /// reorder. Not a model of the engine, which does its own work now — that is why it is zero unless
    /// somebody sets it.
    fn injected_delay(&mut self) -> u64 {
        self.latency.sample(&mut self.jitter).as_nanos() as u64
    }

    fn deliver(&mut self) -> bool {
        if !self.results.flush() {
            return false;
        }
        let now = self.now();
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
}
