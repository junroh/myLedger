use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ledger_base::ports::HoldData;
use ledger_base::ports::{
    OverlayState, PendingCommand, PendingEffect, PendingLookup, PendingNotice, PendingOverlay,
    PendingPort, PendingReply,
};
use ledger_base::BudgetGroup;
use ledger_base::{
    channel, Amount, Consumer, Footprint, FxHashMap, LedgerError, MapGauge, Prng, Producer,
    StagedProducer, Transfer, TxId,
};
use ledger_stubkit::{AnswerGate, IdleBackoff, LatencyRange, WorkerThread};

use crate::block::{
    LogTraffic, OpenBacking, RecordAddr, StoreModel, VolumeStats, BLOCK_BYTES, RECORDS_PER_BLOCK,
    SEGMENTS,
};
use crate::cache::Cached;
use crate::engine::{BudgetState, PendingEngine, Started};
use crate::index::{LOAD_TARGET, SLOT_BYTES};
use crate::orderer::OrderWait;
use crate::orderer::Orderer;
use crate::overlay::HoldOverlay;
use crate::snapshots::{SnapshotPolicy, SnapshotStats, Snapshots};

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
    /// Blocks of an expiring day the sweep reads per round, so a day's worth spreads over the day instead
    /// of arriving as one burst that competes with live traffic. Falling behind deletes late, which is
    /// safe, so this trades promptness for headroom rather than correctness for speed.
    ///
    /// Blocks rather than voids, and the unit is the point: a bound on voids collected is no bound on the
    /// work done to collect them, which is how the index scan this replaced came to cost 2.2 seconds a pass
    /// at the design's size. Blocks bound both — the voids one round can offer are at most this many times
    /// `RECORDS_PER_BLOCK`, which is also what the notice queue has to absorb before the sweep is asked
    /// again.
    pub expiry_blocks_per_round: usize,

    /// Blocks of a volume's cold read cache — see `Cached`. **Zero is no cache**, which is the baseline
    /// any number here is compared against.
    ///
    /// A count rather than a share of anything, because what it has to cover is a burst of reads into one
    /// block and that is a property of the traffic rather than of the ledger's declared sizes. Sixty-four
    /// blocks is 256KB, which is small enough not to be a tier and large enough that other traffic does
    /// not evict an expiry slice out from under itself.
    pub read_cache_blocks: usize,

    /// How often a snapshot is written, how fast, and how much the stable read may hold aside while it
    /// is. `every: 0` writes none, which is the default: where one goes is a directory the caller opens,
    /// so a configuration alone can never make a node write files nobody asked for.
    pub snapshot: SnapshotPolicy,
}

/// Where the engine gets the day retention is measured in.
///
/// Wall time rather than the monotonic clock the rest of the engine runs on: retention is a promise in
/// calendar terms and has to survive a restart, which an origin-relative clock cannot do. Read to the day,
/// so a clock that drifts by minutes changes nothing and one that jumps forward is absorbed by
/// `grace_days`.
///
/// Injectable for the same reason `Clock` is: a retention window is measured in days, and a test or a
/// simulation that could only reach the end of one by waiting would never reach it at all.
#[derive(Clone)]
pub enum DaySource {
    WallClock,
    /// Moved by hand. Shared, so the caller advances it while the worker reads it.
    Fixed(Arc<AtomicU64>),
}

impl DaySource {
    /// A day source the caller can move, and the handle to move it with.
    pub fn manual(day: u64) -> (Self, Arc<AtomicU64>) {
        let shared = Arc::new(AtomicU64::new(day));
        (Self::Fixed(Arc::clone(&shared)), shared)
    }

    fn today(&self) -> u64 {
        match self {
            Self::WallClock => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs() / 86_400)
                .unwrap_or(0),
            Self::Fixed(day) => day.load(Ordering::Relaxed),
        }
    }
}

/// What the engine has on disk: the blocks its records live on, and where its snapshots go.
///
/// **Two backings, because they answer to different volumes.** The design puts the Raft log and the
/// snapshot on Disk 1 and the pending blocks elsewhere (§2.2), and nothing in this engine depends on that
/// being the layout — a throttled dump competes with the log's commits on a shared volume and with the
/// engine's own reads on a separate one, and the throttle is required either way. Taking two backings is
/// what keeps the choice a provisioning decision instead of one this code made by accident.
///
/// **And when the two name one volume they get one store.** IO into a disk has to be managed and watched
/// in one place whoever asked for it, so a queue depth belongs to the device rather than to the writer
/// (§20). What decides that they are one volume is `OpenBacking::same_volume`, which today can only
/// recognise the case it cannot be wrong about — the same directory. Two directories on one disk is a
/// declaration this repository has not built yet, and it is on `status.md` rather than guessed at here.
///
/// Handles rather than paths, and opened by the caller: a directory that cannot be used is a
/// configuration error, and one discovered on the worker's thread has nowhere to be reported (rule 6).
pub struct PendingStorage {
    pub blocks: OpenBacking,
    /// Where a snapshot goes. `None` writes none, whatever the policy says — the two have to agree for a
    /// node to write files, so neither alone can make it.
    pub snapshots: Option<OpenBacking>,
}

impl PendingStorage {
    /// Memory, and no snapshots. What every number in the documents was taken against, and what a run
    /// gets unless it asked for otherwise.
    pub fn memory() -> Self {
        Self {
            blocks: OpenBacking::Memory,
            snapshots: None,
        }
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
    /// How long a hold's record is kept. A promise to the customer with two edges: keeping it longer
    /// breaks the deletion promise, and deleting it sooner refuses a resolution that was still entitled
    /// to arrive. The second is a wrong answer, so expiry errs late — see `grace_days`.
    pub retention_days: u64,
    /// Days of slack added before a record is deleted, so deletion is never early.
    ///
    /// Records are deleted a whole segment at a time and a segment is a day, so a hold created at
    /// 23:59:59 shares its segment with one created at 00:00:00. Without slack the younger one would be
    /// deleted a day short of its retention. One day of slack costs a day of capacity — the index and
    /// the store are sized for `retention + grace` — and buys away every source of *early* deletion at
    /// once: the segment's own coarseness, a wall clock jumping forward, and a sweep that has not run
    /// yet. Raising it is the answer to any of them; the price is linear and only ever space.
    pub grace_days: u64,
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
            grace_days: 1,
            survives_flush_window: 0.5,
            flush_window_hours: 1,
            residency_hours: 24,
        }
    }
}

impl PendingCapacity {
    /// Days a record may live: what was promised, plus the slack that keeps deletion from being early.
    /// Every size that follows from a lifetime uses this rather than `retention_days` — the index sized
    /// for the promise alone would be a day short of what the grace lets live, and the declared maximum
    /// would go back to being a hope.
    pub fn lifetime_days(&self) -> u64 {
        self.retention_days + self.grace_days
    }

    /// Segments live at once: one per day of lifetime, plus the day being filled.
    pub fn live_segments(&self) -> u64 {
        self.lifetime_days() + 1
    }

    /// Holds alive at once in the worst case the configuration declares.
    pub fn declared_maximum(&self) -> u64 {
        (self.daily_arrivals as f64 * self.worst_survivor_share) as u64 * self.lifetime_days()
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
    /// Buffered blocks one round may carry out.
    ///
    /// **Derived from the queue, because that is the closest thing to a bound on a round's arrivals.** A
    /// round drains the command queue, so a queue's worth of applies is a queue's worth of records and so
    /// that many blocks over `RECORDS_PER_BLOCK`.
    ///
    /// **It is not a guarantee that the drain keeps up, and the first version of this comment said it was.**
    /// A round can carry *more* than `queue_capacity` commands, because the sequencer refills the queue
    /// while the round is emptying it — `drain_commands` pops until the queue is empty, not until it has
    /// popped a capacity's worth. So nothing bounds a round's applies, the buffer can pass its ceiling
    /// inside one round, and the stall below is what catches it rather than an impossibility. Measured on
    /// `partial-settle` at 1M/s for five seconds: 4.58M applies, 89,071 blocks drained, **8 stalls** — rare
    /// enough to cost nothing and not zero, which is the difference between a bound and a hope.
    ///
    /// Set beside `queue_capacity` instead of derived from it, the two could disagree, and the failure
    /// would be a buffer that grows past the window it was sized for — silent, and visible only as recovery
    /// taking longer than the flush window promised.
    pub fn drain_blocks_per_round(&self) -> usize {
        self.queue_capacity.div_ceil(RECORDS_PER_BLOCK).max(1)
    }

    /// Blocks of the store's queue a snapshot dump may hold at once.
    ///
    /// Half, and the half is what the blocks are guaranteed: the ledger can always place a queue's worth
    /// less the dump's share without waiting for a background job's completion. Derived rather than
    /// configured beside the depth, because a share set next to the number it is a share of is a pair
    /// that can disagree — and the consequence of it disagreeing is a client refused for a snapshot.
    pub fn snapshot_queue_share(&self) -> usize {
        (self.store.queue_depth.max(1) / 2).max(1)
    }

    /// Blocks the writeback buffer may hold before applying stops.
    ///
    /// The window plus one round's drain: the window is what recovery is sized for, and the slack is what
    /// lets a round's arrivals land before the drain that follows them in the same round carries them out.
    ///
    /// Reaching it is ordinary rather than alarming — a round is not bounded by the queue's capacity, so a
    /// busy one overshoots by a little and the next round's drain clears it, at the cost of one round's
    /// delay for one command. What it is *not* is a rate limit on the ledger: the applies stall only while
    /// the buffer is over the ceiling, and the drain that follows in the same round is what ends it. A
    /// stall count that climbs with the run is the drain genuinely behind, which is a slow store.
    pub fn buffer_ceiling(&self) -> usize {
        self.capacity.flush_blocks() + self.drain_blocks_per_round()
    }

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
            // Deletion is a whole segment at a time and a segment is a day, so without slack the
            // youngest hold in a segment would be deleted a day short of the retention it was promised.
            && capacity.grace_days > 0
            // A segment's number is its day modulo the segments available, so a lifetime that needs
            // more of them than exist would give two live days one number, and expiry would delete the
            // wrong day's records. Refused here rather than discovered as lost holds.
            && capacity.live_segments() <= SEGMENTS
            // Residency shorter than the flush window would mean records leaving memory before they are
            // written, which is not a window at all.
            && capacity.residency_hours >= capacity.flush_window_hours
            // Nor longer than a record is allowed to exist: keeping one in memory past the day its blocks
            // are handed back would leave residency answering from a block the store no longer has.
            && capacity.residency_hours <= capacity.lifetime_days() * 24
            && capacity.slots() * SLOT_BYTES <= self.index_budget_bytes
            // A round that cannot write one block would never finish a dump, and a shadow budget of
            // nothing gives up on every dump the first time anything is written behind it. Both are
            // configurations that look like a snapshot policy and produce no snapshot, which is exactly
            // what `validate` exists to refuse. A throttle that is not a whole number of blocks is
            // refused rather than rounded: the store takes blocks, and rounding a declared number into a
            // different one is how a configuration comes to mean something nobody asked for.
            && (self.snapshot.every == 0
                || (self.snapshot.bytes_per_round >= BLOCK_BYTES
                    && self.snapshot.bytes_per_round.is_multiple_of(BLOCK_BYTES)
                    && self.snapshot.shadow_budget > 0));
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
            // Two blocks, so a round offers at most a hundred-odd voids — about what the sixty-four of the
            // scan it replaces did, and the notice queue is sized against that rather than against a
            // number chosen for its own sake.
            read_cache_blocks: 64,
            expiry_blocks_per_round: 2,
            store: StoreModel {
                queue_depth: 128,
                ..StoreModel::default()
            },
            snapshot: SnapshotPolicy::default(),
        }
    }
}

/// Depth of the notice queue. Small on purpose: a notice is rare, and the worker latches one it
/// cannot hand over, so depth is promptness rather than safety.
const NOTICE_QUEUE: usize = 64;

/// In-memory tier of the pending engine: it stores what the sequencer committed and
/// provides what a settle or void asks for, and judges nothing. The disk tier for holds
/// that outlive memory is not built yet.
pub struct MemoryPending {
    commands: Producer<PendingCommand>,
    results: Consumer<PendingReply>,
    /// What the engine says without being asked. A queue of its own, because a notice answers no
    /// command and must not sit behind — or delay — a reply that a request is waiting for.
    notices: Consumer<PendingNotice>,
    /// Read inline, on the caller's own thread.
    overlay: HoldOverlay,
    /// Applies handed to the engine. A removal's marker is stamped with this so the overlay knows when
    /// the engine has caught up with it; the engine counts the same sequence as it applies them.
    applies_sent: u64,
    /// What the store is holding, published by the worker because the store lives on its thread.
    occupancy: Arc<Occupancy>,
    /// Block buffers the volume took at start-up and holds for the life of the node: the read cache, and
    /// the queues' own, one per slot they may hold. Fixed, so they are numbers rather than gauges — and
    /// reported for the same reason everything else is, that an absent line reads as a free one.
    read_cache_blocks: usize,
    lane_blocks: usize,
    read_pool_blocks: usize,
    /// A test's hold on the replies — see `AnswerGate`. Open unless somebody closed it.
    replies: AnswerGate,
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
    carried_on: AtomicU64,
    freed: AtomicU64,
    left_memory: AtomicU64,
    buffer_reads: AtomicU64,
    resident_reads: AtomicU64,
    store_reads: AtomicU64,
    apply_store_reads: AtomicU64,
    index_live: AtomicU64,
    index_slots: AtomicU64,
    worst_cascade: AtomicU64,
    ambiguous: AtomicU64,
    overflowed: AtomicU64,
    segment: AtomicU64,
    days_behind: AtomicU64,
    days_of_slack: AtomicU64,
    swept_blocks: AtomicU64,
    store_faults: AtomicU64,
    store_corruptions: AtomicU64,
}

impl TrafficGauge {
    fn publish(&self, traffic: LogTraffic) {
        self.appended.store(traffic.appended, Ordering::Relaxed);
        self.died_in_buffer
            .store(traffic.died_in_buffer, Ordering::Relaxed);
        self.carried_on.store(traffic.carried_on, Ordering::Relaxed);
        self.freed.store(traffic.freed, Ordering::Relaxed);
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
        self.index_live
            .store(traffic.index_live as u64, Ordering::Relaxed);
        self.index_slots
            .store(traffic.index_slots as u64, Ordering::Relaxed);
        self.worst_cascade
            .store(u64::from(traffic.worst_cascade), Ordering::Relaxed);
        self.ambiguous.store(traffic.ambiguous, Ordering::Relaxed);
        self.overflowed.store(traffic.overflowed, Ordering::Relaxed);
        self.segment
            .store(u64::from(traffic.segment), Ordering::Relaxed);
        self.days_behind
            .store(traffic.days_behind, Ordering::Relaxed);
        self.days_of_slack
            .store(traffic.days_of_slack, Ordering::Relaxed);
        self.swept_blocks
            .store(traffic.swept_blocks, Ordering::Relaxed);
        self.store_faults
            .store(traffic.store_faults, Ordering::Relaxed);
        self.store_corruptions
            .store(traffic.store_corruptions, Ordering::Relaxed);
    }

    fn read(&self) -> LogTraffic {
        LogTraffic {
            appended: self.appended.load(Ordering::Relaxed),
            died_in_buffer: self.died_in_buffer.load(Ordering::Relaxed),
            carried_on: self.carried_on.load(Ordering::Relaxed),
            freed: self.freed.load(Ordering::Relaxed),
            left_memory: self.left_memory.load(Ordering::Relaxed),
            buffer_reads: self.buffer_reads.load(Ordering::Relaxed),
            resident_reads: self.resident_reads.load(Ordering::Relaxed),
            store_reads: self.store_reads.load(Ordering::Relaxed),
            apply_store_reads: self.apply_store_reads.load(Ordering::Relaxed),
            index_live: self.index_live.load(Ordering::Relaxed) as usize,
            index_slots: self.index_slots.load(Ordering::Relaxed) as usize,
            worst_cascade: self.worst_cascade.load(Ordering::Relaxed) as u32,
            ambiguous: self.ambiguous.load(Ordering::Relaxed),
            overflowed: self.overflowed.load(Ordering::Relaxed),
            segment: self.segment.load(Ordering::Relaxed) as u8,
            days_behind: self.days_behind.load(Ordering::Relaxed),
            days_of_slack: self.days_of_slack.load(Ordering::Relaxed),
            swept_blocks: self.swept_blocks.load(Ordering::Relaxed),
            store_faults: self.store_faults.load(Ordering::Relaxed),
            store_corruptions: self.store_corruptions.load(Ordering::Relaxed),
        }
    }
}

/// The store's occupancy as the worker last published it.
/// One volume's numbers across the thread boundary. Two of them, because a node may have two volumes and
/// "how deep did the queue get" has no answer that spans a disk.
#[derive(Debug, Default)]
struct VolumeGauge {
    reads_submitted: AtomicU64,
    reads_answered: AtomicU64,
    reads_inline: AtomicU64,
    reads_cached: AtomicU64,
    reads_joined: AtomicU64,
    writes: AtomicU64,
    barriers: AtomicU64,
    removes: AtomicU64,
    renames: AtomicU64,
    bytes_written: AtomicU64,
    reads_refused: AtomicU64,
    writes_refused: AtomicU64,
    read_depth_peak: AtomicU64,
    write_depth_peak: AtomicU64,
    faults: AtomicU64,
    /// Whether anything published here at all. A volume of its own that never existed reads as one that
    /// did nothing, and the two are worth telling apart.
    present: AtomicBool,
}

impl VolumeGauge {
    fn publish(&self, stats: VolumeStats) {
        self.present.store(true, Ordering::Relaxed);
        self.reads_submitted
            .store(stats.reads_submitted, Ordering::Relaxed);
        self.reads_answered
            .store(stats.reads_answered, Ordering::Relaxed);
        self.reads_inline
            .store(stats.reads_inline, Ordering::Relaxed);
        self.reads_cached
            .store(stats.reads_cached, Ordering::Relaxed);
        self.reads_joined
            .store(stats.reads_joined, Ordering::Relaxed);
        self.writes.store(stats.writes, Ordering::Relaxed);
        self.barriers.store(stats.barriers, Ordering::Relaxed);
        self.removes.store(stats.removes, Ordering::Relaxed);
        self.renames.store(stats.renames, Ordering::Relaxed);
        self.bytes_written
            .store(stats.bytes_written, Ordering::Relaxed);
        self.reads_refused
            .store(stats.reads_refused, Ordering::Relaxed);
        self.writes_refused
            .store(stats.writes_refused, Ordering::Relaxed);
        self.read_depth_peak
            .store(stats.read_depth_peak as u64, Ordering::Relaxed);
        self.write_depth_peak
            .store(stats.write_depth_peak as u64, Ordering::Relaxed);
        self.faults.store(stats.faults, Ordering::Relaxed);
    }

    fn read(&self) -> Option<VolumeStats> {
        if !self.present.load(Ordering::Relaxed) {
            return None;
        }
        Some(VolumeStats {
            reads_submitted: self.reads_submitted.load(Ordering::Relaxed),
            reads_answered: self.reads_answered.load(Ordering::Relaxed),
            reads_inline: self.reads_inline.load(Ordering::Relaxed),
            reads_cached: self.reads_cached.load(Ordering::Relaxed),
            reads_joined: self.reads_joined.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            barriers: self.barriers.load(Ordering::Relaxed),
            removes: self.removes.load(Ordering::Relaxed),
            renames: self.renames.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            reads_refused: self.reads_refused.load(Ordering::Relaxed),
            writes_refused: self.writes_refused.load(Ordering::Relaxed),
            read_depth_peak: self.read_depth_peak.load(Ordering::Relaxed) as usize,
            write_depth_peak: self.write_depth_peak.load(Ordering::Relaxed) as usize,
            faults: self.faults.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug)]
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
    /// Committed decisions the engine has applied. Published for the overlay on the other thread: a
    /// removal's marker may go once this passes the point the removal was handed over at, and not
    /// before — see `HoldOverlay::forget`.
    applied: AtomicU64,
    /// What putting each lane back in order cost. Published because it is the one cost no per-read bound
    /// covers, and until now the engine computed it and nobody could read it.
    order_wait: OrderWaitGauge,
    /// Buffered blocks the drain has carried out, and applies refused because it had not. The first is the
    /// number the drain's move exists to produce — inside apply it could not be separated from apply's own
    /// time — and the second is the failure mode the move created.
    drained_blocks: AtomicU64,
    buffer_stalls: AtomicU64,
    /// Whether the sequencer has room for more expiry voids, as it last said. The sweep offers nothing while
    /// this is false: a declined void is re-offered by the sweep and by nothing else, so without a pause here
    /// a full backlog becomes a re-offer every round. Advisory — a stale read costs one wasted offer.
    wants_expiry: AtomicBool,
    /// What the snapshot stage has written, given up on, and held aside. Zero throughout when no
    /// directory was named, which is how a report says "this run wrote none" rather than saying nothing.
    snapshots: SnapshotGauge,
    /// The blocks' volume, and the snapshot's when it is one of its own.
    blocks_volume: VolumeGauge,
    snapshot_volume: VolumeGauge,
}

impl Default for Occupancy {
    /// `wants_expiry` starts true and every other field at zero. Derived would start it false, which would
    /// stop the sweep until the sequencer's first tick had spoken — a pause nobody asked for, and one that
    /// would never lift in a test that drives the engine without a reactor.
    fn default() -> Self {
        Self {
            holds: MapGauge::default(),
            budgets: MapGauge::default(),
            blocks: MapGauge::default(),
            buffer: MapGauge::default(),
            resident: MapGauge::default(),
            traffic: TrafficGauge::default(),
            applied: AtomicU64::default(),
            order_wait: OrderWaitGauge::default(),
            drained_blocks: AtomicU64::default(),
            buffer_stalls: AtomicU64::default(),
            wants_expiry: AtomicBool::new(true),
            snapshots: SnapshotGauge::default(),
            blocks_volume: VolumeGauge::default(),
            snapshot_volume: VolumeGauge::default(),
        }
    }
}

/// The snapshot stage's six numbers across the thread boundary, for the same reason the orderer's four
/// are a gauge: they are the worker's and a report is on the other thread.
#[derive(Debug, Default)]
struct SnapshotGauge {
    written: AtomicU64,
    abandoned: AtomicU64,
    bytes: AtomicU64,
    shadow_peak: AtomicU64,
    last_rounds: AtomicU64,
    covered: AtomicU64,
}

impl SnapshotGauge {
    fn publish(&self, stats: SnapshotStats) {
        self.written.store(stats.written, Ordering::Relaxed);
        self.abandoned.store(stats.abandoned, Ordering::Relaxed);
        self.bytes.store(stats.bytes, Ordering::Relaxed);
        self.shadow_peak.store(stats.shadow_peak, Ordering::Relaxed);
        self.last_rounds.store(stats.last_rounds, Ordering::Relaxed);
        self.covered.store(stats.covered, Ordering::Relaxed);
    }

    fn read(&self) -> SnapshotStats {
        SnapshotStats {
            written: self.written.load(Ordering::Relaxed),
            abandoned: self.abandoned.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            shadow_peak: self.shadow_peak.load(Ordering::Relaxed),
            last_rounds: self.last_rounds.load(Ordering::Relaxed),
            covered: self.covered.load(Ordering::Relaxed),
        }
    }
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
        Self::start_with_days(config, DaySource::WallClock, PendingStorage::memory())
    }

    /// The same, with the day and the storage handed in.
    ///
    /// Both come from outside rather than out of the configuration, and for the same reason: a retention
    /// window is measured in days, so a test that had to wait for one would never exercise expiry at all, and
    /// a directory is a resource that has to be opened and checked before a thread owns it. `MemoryPendingConfig`
    /// stays `Copy` because neither is in it.
    pub fn start_with_days(
        config: MemoryPendingConfig,
        days: DaySource,
        storage: PendingStorage,
    ) -> Result<Self, LedgerError> {
        config.validate()?;
        let (commands, command_rx) = channel(config.queue_capacity);
        let (result_tx, results) = channel(config.queue_capacity);
        let (notice_tx, notices) = channel(NOTICE_QUEUE);
        let occupancy = Arc::new(Occupancy::default());
        let worker_occupancy = Arc::clone(&occupancy);
        let replies = AnswerGate::default();
        let worker_replies = replies.clone();
        let PendingStorage { blocks, snapshots } = storage;
        // One store when the two name one volume, and the `Option` the stage takes is exactly that
        // question: dropping the backing here is what says the blocks' store is this dump's store too.
        let dumps = snapshots.is_some();
        let apart = snapshots.filter(|backing| !backing.same_volume(&blocks));
        let depths = config.store.depths();
        let share = config.snapshot_queue_share();
        // Read before the backing is moved into the thread: what the queues will take is a property of
        // what was asked for, and after this it belongs to a store on the other side of a boundary.
        let (lane_blocks, pool_blocks) = match &blocks {
            OpenBacking::Files {
                read_threads,
                write_lane,
                ..
            } => (
                if *write_lane { depths.write } else { 0 },
                if *read_threads > 0 { depths.read } else { 0 },
            ),
            OpenBacking::Memory => (0, 0),
        };
        let thread = WorkerThread::spawn("pending", move |shutdown| {
            PendingWorker {
                commands: command_rx,
                results: StagedProducer::new(result_tx),
                engine: PendingEngine::sized(
                    config.capacity.slots(),
                    config.capacity.flush_blocks(),
                    config.capacity.resident_blocks(),
                    // **Above the model, and that is the point of it being a store of its own.** A hit
                    // never reached a device, so it must not be charged as one; below the model it would
                    // be. `Cached::new(.., 0)` is the baseline and costs one indirection.
                    Box::new(Cached::new(
                        config.store.build(blocks, config.seed ^ 0xb10c),
                        config.read_cache_blocks,
                    )),
                ),
                // A volume of its own is opened exact, with no device modelled in front of it: the
                // `--store-*` knobs describe the blocks' device, and pricing a second disk with the first
                // one's numbers would be claiming the two are the same — the guess §20 refuses in the
                // other direction. A snapshot on the modelled device is what declaring one volume gets.
                snapshots: dumps.then(|| {
                    Snapshots::new(
                        apart.map(|backing| backing.open(depths)),
                        config.snapshot,
                        share,
                    )
                }),
                occupancy: worker_occupancy,
                replies: worker_replies,
                notices: notice_tx,
                owed: VecDeque::new(),
                expiring: Vec::new(),
                lifetime_days: config.capacity.lifetime_days(),
                expiry_blocks_per_round: config.expiry_blocks_per_round,
                drain_blocks_per_round: config.drain_blocks_per_round(),
                buffer_ceiling: config.buffer_ceiling(),
                buffer_stalls: 0,
                days,
                orderer: Orderer::new(config.violate_order_every),
                stale_answer_every: config.stale_answer_every,
                answers: 0,
                inflight: FxHashMap::default(),
                handles: 0,
                deferred: None,
                jitter: Prng::new(config.seed),
                latency: config.latency,
                started: Instant::now(),
                busy_until: 0,
            }
            .run(shutdown)
        });
        Ok(Self {
            commands,
            results,
            notices,
            overlay: HoldOverlay::new(
                config.queue_capacity,
                config.overlay_soft_limit,
                config.eviction_per_round,
            ),
            applies_sent: 0,
            occupancy,
            read_cache_blocks: config.read_cache_blocks,
            lane_blocks,
            read_pool_blocks: pool_blocks,
            replies,
            _thread: thread,
        })
    }

    /// A hold on the replies, for a test that has to see a request still waiting — see `AnswerGate`.
    pub fn replies(&self) -> AnswerGate {
        self.replies.clone()
    }

    /// Where the reads went, and how much of what was written never had to be written out.
    pub fn traffic(&self) -> LogTraffic {
        self.occupancy.traffic.read()
    }

    /// What keeping each lane in seq order cost on top of the reads themselves.
    pub fn order_wait(&self) -> OrderWait {
        self.occupancy.order_wait.read()
    }

    /// What the drain carried out and what it cost the applies behind it: blocks drained, and applies
    /// refused because the buffer was over its ceiling. The second must stay at zero on any run that is
    /// measuring the ledger — it means the drain fell behind, which is the one failure the drain's move out
    /// of apply made possible (§20).
    pub fn drain_work(&self) -> (u64, u64) {
        (
            self.occupancy.drained_blocks.load(Ordering::Relaxed),
            self.occupancy.buffer_stalls.load(Ordering::Relaxed),
        )
    }

    /// What the snapshot stage has done: dumps published and given up on, bytes, the shadow's peak, and
    /// where the last published one covered to.
    pub fn snapshots(&self) -> SnapshotStats {
        self.occupancy.snapshots.read()
    }

    /// What each volume did, counted by the volume. The second is `None` when one store serves both,
    /// because then the first covers everything on that disk.
    pub fn volumes(&self) -> (VolumeStats, Option<VolumeStats>) {
        (
            self.occupancy.blocks_volume.read().unwrap_or_default(),
            self.occupancy.snapshot_volume.read(),
        )
    }

    /// What this engine is holding: the store on the worker's thread, and the overlay on the
    /// caller's. Both are memory — the disk tier for holds that outlive memory is not built, so
    /// nothing here is a disk figure.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        // The index is what a hold costs in memory; the record itself lives on a block, and the blocks
        // are where the store's own size is. Both are memory today — the disk tier is not built — so
        // neither figure is a disk figure.
        footprint.gauged_table::<TxId, RecordAddr>("engine index", &self.occupancy.holds);
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
        // Taken at start-up whether or not it is used, so entries and peak are the same number. No
        // capacity is passed: a fixed allocation is not a ceiling something reached, and reporting it as
        // one would put it in the list of things a run should worry about.
        for (name, blocks) in [
            ("volume read cache", self.read_cache_blocks),
            ("volume write lane", self.lane_blocks),
            ("volume read pool", self.read_pool_blocks),
        ] {
            if blocks == 0 {
                continue;
            }
            footprint.other(name, blocks, blocks, 0, blocks * BLOCK_BYTES);
        }
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
        //
        // The count is this side's half of "has the engine caught up": it numbers the applies handed
        // over, the engine numbers the same ones as it applies them, and a removal's marker is stamped
        // with its own number so it can be dropped when the engine reaches it.
        if matches!(command, PendingCommand::Apply { .. }) {
            self.applies_sent += 1;
        }
        match command {
            PendingCommand::Apply {
                effect: PendingEffect::Create { tx_id, amount, .. },
                ..
            } => self.overlay.created(tx_id, amount),
            PendingCommand::Apply {
                effect:
                    PendingEffect::Reduce {
                        pending_ref,
                        remaining,
                        ..
                    },
                ..
            } => self.overlay.note_remaining(pending_ref, remaining),
            PendingCommand::Apply {
                effect: PendingEffect::Remove { pending_ref, .. },
                ..
            } => self.overlay.forget(pending_ref, self.applies_sent),
            _ => {}
        }
        Ok(())
    }

    fn poll(&self) -> Option<PendingReply> {
        self.results.pop()
    }

    fn notices(&self) -> Option<PendingNotice> {
        self.notices.pop()
    }

    fn set_wants_expiry(&mut self, wanted: bool) {
        self.occupancy.wants_expiry.store(wanted, Ordering::Relaxed);
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
        self.overlay
            .maintain(self.occupancy.applied.load(Ordering::Relaxed))
    }

    fn overlay_len(&self) -> usize {
        self.overlay.len()
    }
}

/// The group as the engine knows it lives with the engine — see `engine.rs`.
///
/// The field is `engine` and not `store`: `DurableStore` is the store now, one layer further down, and one
/// word for two things is what rule 3 forbids.
struct PendingWorker {
    commands: Consumer<PendingCommand>,
    results: StagedProducer<PendingReply>,
    engine: PendingEngine,
    /// Where a snapshot goes and what paces it there, absent when no directory was named. The stage owns
    /// its own state (rule 11); the worker owes it a round.
    snapshots: Option<Snapshots>,
    occupancy: Arc<Occupancy>,
    /// A test's hold on the replies — see `AnswerGate`. Open unless somebody closed it.
    replies: AnswerGate,
    /// The engine's own end of the notice channel.
    notices: Producer<PendingNotice>,
    /// Notices the queue would not take yet. Expiry is what makes this a queue rather than one slot: a
    /// seal is the same news however often it is said, but every expired hold is a different one, and a
    /// dropped one would leave a pending column reserved for good.
    owed: VecDeque<PendingNotice>,
    /// Voids the sweep has found and not handed over. Reused, so a sweep round allocates nothing.
    expiring: Vec<Transfer>,
    /// How long a record may live and how many expiry voids one round may offer. The second is what
    /// keeps a day's expiry from arriving as a burst; falling behind deletes late, which is safe.
    lifetime_days: u64,
    expiry_blocks_per_round: usize,
    /// Buffered blocks one round may carry out, and the blocks the buffer may hold before applying stops.
    /// Both derived rather than set (`MemoryPendingConfig`), because the second only holds if the first
    /// keeps up with a full command queue.
    drain_blocks_per_round: usize,
    buffer_ceiling: usize,
    /// Applies refused because the buffer was over its ceiling. The drain falling behind, counted — it is
    /// the failure mode the move created, so a run has to be able to say it never happened.
    buffer_stalls: u64,
    days: DaySource,
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
    /// When a modelled device's last synchronous call lets this thread go. Zero unless a device is modelled.
    busy_until: u64,
}

impl PendingWorker {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut backoff = IdleBackoff::new();
        while !shutdown.load(Ordering::Relaxed) {
            // A modelled device's synchronous calls hold this thread, and nothing else it does is available
            // while they do — which is the whole point of charging them here rather than to a queue. With no
            // device modelled the deadline is never set and this is one comparison.
            if self.now() < self.busy_until {
                backoff.record(false);
                continue;
            }
            // One reading of the clock for the round, so every stage in it prices against the same
            // instant — a store that models a device is handed this the way it already was for reads.
            let now = self.now();
            let progress = self.hand_over_notices()
                | self.sweep_expiry(now)
                | self.drain_commands()
                | self.harvest()
                // Writes and barriers the store has answered: blocks reach residency and a completed
                // barrier moves coverage. Beside `harvest` because it is the same job for the other
                // direction, and before the drain below so this round's submissions find room.
                | self.engine.collect_writes(now)
                | self.deliver()
                // After the replies are out and before the sync, and both edges are the contract. This
                // round's appends have to be in the buffer for the drain to see them, and what it seals
                // has to be there for the sync below to cover. Applying no longer does this (§20).
                | self.engine.drain(self.drain_blocks_per_round, now)
                // Last, so one sync covers every block this round sealed — group commit within a round. The
                // policy is here because it is a policy: syncing less often costs coverage and nothing else,
                // and what it buys back is a device's fsync off the thread that answers lookups.
                // `status.md`'s decisions list has that trade and what would settle it.
                | self.engine.sync(now)
                // After the sync, because a snapshot may carry only what a crash would find: a dump that
                // began before it would take this round's seals as still-unwritten and leave their slots
                // out. Costs nothing but a fresher coverage, and there is no reason to give that up.
                | self.snapshot_round(now);
            // Taken after the round rather than before, because the round is what incurred it: the writes,
            // syncs and apply-path reads it just did are time this thread would have been inside a syscall
            // for. Absolute, so a real device under the model has already spent it and the gate is a no-op.
            if self.engine.take_store_fault() {
                self.owe(PendingNotice::StoreFailed);
                // The apply path is about to be sealed, so nothing more will be applied and a dump of a
                // node that has stopped is bytes nobody will read. Given up on here rather than left to
                // finish, because finishing it would keep the shadow and the file writes going for the
                // whole of a state that is no longer moving.
                if let Some(snapshots) = self.snapshots.as_mut() {
                    snapshots.abandon(&mut self.engine);
                }
            }
            let owed = self.engine.take_store_charge();
            if owed > 0 {
                self.busy_until = self.now() + owed;
            }
            if progress {
                self.publish();
            }
            // The engine's own numbers, checked every round in constant time (rule 6). Drifted counts do
            // not lose money — the slots are right and only their summary is wrong — so this asserts in a
            // debug build and does nothing in a release one, where the consequence surfaces as
            // `days_behind` climbing and then as an index that cannot take an insert.
            debug_assert!(
                self.engine.counts_agree(),
                "the index's per-segment counts no longer add up to its entries"
            );
            backoff.record(progress);
        }
    }

    /// This round's share of a snapshot, and nothing at all when no directory was named. The stage keeps
    /// its own cadence and its own throttle; the worker only owes it a turn.
    fn snapshot_round(&mut self, now: u64) -> bool {
        let Some(snapshots) = self.snapshots.as_mut() else {
            return false;
        };
        snapshots.round(&mut self.engine, now)
    }

    /// First in the round, and it retries until each notice lands: news the sequencer has to act on may
    /// not be dropped because a queue was momentarily full.
    fn hand_over_notices(&mut self) -> bool {
        let mut progress = false;
        while let Some(notice) = self.owed.front() {
            if self.notices.push(*notice).is_err() {
                break;
            }
            self.owed.pop_front();
            progress = true;
        }
        progress
    }

    /// Queued rather than sent from where it was found, so the one place that pushes is the one place that
    /// retries.
    fn owe(&mut self, notice: PendingNotice) {
        self.owed.push_back(notice);
    }

    /// Moves the day on when the wall clock says it has, hands back the blocks of any day nothing points
    /// into, and offers a bounded slice of whatever ran out.
    ///
    /// **The three are not the same job and the middle one is not the leader's.** Reclaiming needs no clock
    /// and no consensus — a segment the index has no entry in holds only dead records — so every node does
    /// it for itself. Proposing an expiry void is the leader's, because a proposal needs somewhere to go.
    /// Once consensus is real this method splits along that line; today there is one node and the split is
    /// stated rather than enforced, which is why `reclaim` is called before the leader-only part and not
    /// inside it.
    ///
    /// Wall time, not the monotonic clock the rest of this file runs on: retention is a promise in calendar
    /// terms and has to survive a restart, which an origin-relative clock cannot do. Read once a round and
    /// only to the day, so the cost is nothing and a jump forward is absorbed by `grace_days`.
    fn sweep_expiry(&mut self, now: u64) -> bool {
        let day = self.days.today();
        let opened = self.engine.open_day(day, self.lifetime_days);
        let reclaimed = self.engine.reclaim() > 0;
        if !self.engine.sweeping() {
            return opened || reclaimed;
        }
        // Owed notices are still waiting, so the sequencer has not kept up with the last slice; offering
        // more would grow this queue rather than release anything sooner.
        if !self.owed.is_empty() {
            return opened || reclaimed;
        }
        // And the sequencer's own backlog is full, so a slice offered now would be declined a void at a
        // time. The sweep is the only thing that retries a declined void, so offering into a full backlog
        // is how a retry becomes a loop: it costs a slot and a lane place per void, per round, to be told
        // no again. Rule 12's pause, for the backlog no client fills.
        if !self.occupancy.wants_expiry.load(Ordering::Relaxed) {
            return opened || reclaimed;
        }
        self.expiring.clear();
        let mut found = std::mem::take(&mut self.expiring);
        self.engine
            .propose_expiry(self.expiry_blocks_per_round, now, &mut found);
        for void in &found {
            self.owed
                .push_back(PendingNotice::HoldExpired { void: *void });
        }
        let offered = !found.is_empty();
        self.expiring = found;
        opened || reclaimed || offered
    }

    /// Once per round rather than once per command: the store's size is asked for by a report at the
    /// end of a run, and paying six atomic stores per write would be a cost per request for it.
    ///
    /// Called from the round itself rather than from whichever stage happened to do the work. A sweep
    /// round moves no command, so a run whose day ran out while the client went quiet published nothing
    /// about the walk it had just spent milliseconds on — the number would have been stale exactly when it
    /// was interesting.
    fn publish(&self) {
        self.engine.publish(
            &self.occupancy.holds,
            &self.occupancy.budgets,
            &self.occupancy.blocks,
            &self.occupancy.buffer,
            &self.occupancy.resident,
        );
        self.occupancy.traffic.publish(self.engine.traffic());
        self.occupancy
            .applied
            .store(self.engine.applied(), Ordering::Relaxed);
        self.occupancy.order_wait.publish(self.orderer.order_wait());
        self.occupancy
            .drained_blocks
            .store(self.engine.drained_blocks(), Ordering::Relaxed);
        self.occupancy
            .buffer_stalls
            .store(self.buffer_stalls, Ordering::Relaxed);
        self.occupancy
            .blocks_volume
            .publish(self.engine.volume_stats());
        if let Some(snapshots) = self.snapshots.as_ref() {
            self.occupancy.snapshots.publish(snapshots.stats());
            if let Some(stats) = snapshots.volume_stats() {
                self.occupancy.snapshot_volume.publish(stats);
            }
        }
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
                    match self.engine.begin_lookup(handle, lookup.pending_ref, now) {
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
                // The one answer an expiry void gets. Without it the sweep cannot tell a void this
                // sequencer refused from one still on its way through consensus, so it retried both —
                // and a retry is a lookup.
                PendingCommand::ExpiryDeclined { hold } => self.engine.expiry_declined(hold),
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
                PendingCommand::Apply { effect, at } => {
                    // The stall the drain's move made necessary. With the producer draining, the window
                    // could not be exceeded — one record in, one record out — and nothing declared that;
                    // it held because of where the call sat (rule 18). Now the drain has a budget, so the
                    // buffer has a ceiling and applying stops at it. Backpressure from here reaches the
                    // client, because a command the engine will not take pauses the sequencer's intake.
                    if self.engine.buffered_blocks() > self.buffer_ceiling
                        || self.engine.writes_outstanding() > self.buffer_ceiling
                    {
                        self.buffer_stalls += 1;
                        self.deferred = Some(command);
                        return false;
                    }
                    if let Err(not_stored) = self.engine.write(effect, at) {
                        self.owe(PendingNotice::HoldNotStored {
                            hold: not_stored.hold,
                        });
                    }
                }
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
        let applied = self.engine.applied();
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
        while let Some((handle, found)) = self.engine.harvest(now) {
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
        // A test holding the replies back. Nothing else stops: the engine keeps applying and the store
        // keeps writing, and only what would leave is kept — which is what makes the hold a state a test
        // can wait on rather than a duration it has to guess.
        if !self.replies.is_open() {
            self.replies.note_waiting(self.orderer.held());
        }
        if !self.results.flush() {
            return false;
        }
        let now = self.now();
        let mut progress = false;
        while !self.results.is_stuck() && self.replies.may_send() {
            match self.orderer.pop_ready(now) {
                Some(result) => {
                    self.replies.spend();
                    self.results.send(result);
                    progress = true;
                }
                None => break,
            }
        }
        progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The promise has two edges and only one of them is safe to miss. Deletion happens a whole segment
    /// at a time and a segment is a day, so every size that follows from a lifetime has to be sized for
    /// `retention + grace`: sized for the promise alone, the index would be a day short of what the grace
    /// lets live, and the declared maximum would go back to being a hope.
    #[test]
    fn every_size_follows_the_lifetime_the_grace_extends() {
        let promised = PendingCapacity {
            daily_arrivals: 1_000_000,
            worst_survivor_share: 0.5,
            retention_days: 32,
            grace_days: 0,
            ..PendingCapacity::default()
        };
        let with_grace = PendingCapacity {
            grace_days: 1,
            ..promised
        };
        assert_eq!(promised.lifetime_days(), 32);
        assert_eq!(with_grace.lifetime_days(), 33);
        assert_eq!(with_grace.declared_maximum(), 500_000 * 33);
        assert!(with_grace.slots() > promised.slots());
    }

    /// A segment's number is its day modulo the segments the address field has, so a lifetime needing
    /// more of them than exist would give two live days one number and expiry would delete the wrong
    /// day's records. Refused at startup rather than discovered as lost holds.
    #[test]
    fn a_lifetime_longer_than_the_segments_available_is_refused() {
        let fits = MemoryPendingConfig {
            capacity: PendingCapacity {
                retention_days: SEGMENTS - 2,
                grace_days: 1,
                ..PendingCapacity::default()
            },
            index_budget_bytes: usize::MAX,
            ..MemoryPendingConfig::default()
        };
        assert_eq!(fits.capacity.live_segments(), SEGMENTS);
        assert_eq!(fits.validate(), Ok(()));

        let one_day_too_many = MemoryPendingConfig {
            capacity: PendingCapacity {
                retention_days: SEGMENTS - 1,
                ..fits.capacity
            },
            ..fits
        };
        assert_eq!(
            one_day_too_many.validate(),
            Err(LedgerError::ConfigInvalid),
            "a lifetime that outruns the segment field was accepted"
        );
    }

    /// Zero grace is what makes deletion early, so it is a configuration error rather than a choice.
    #[test]
    fn no_grace_at_all_is_refused() {
        let config = MemoryPendingConfig {
            capacity: PendingCapacity {
                grace_days: 0,
                ..PendingCapacity::default()
            },
            ..MemoryPendingConfig::default()
        };
        assert_eq!(config.validate(), Err(LedgerError::ConfigInvalid));
    }
}

/// The worker's own wiring: the engine core is unit-tested and the whole stack is covered by the
/// sequencer's integration tests, but what the thread in between does with a day was not.
#[cfg(test)]
mod worker_tests {
    use super::*;
    use crate::block::RECORDS_PER_BLOCK;
    use ledger_base::ports::ApplyIndex;
    use ledger_base::ports::PendingEffect;
    use ledger_base::{AccountId, TransferKind};

    fn config() -> MemoryPendingConfig {
        MemoryPendingConfig {
            capacity: PendingCapacity {
                // An hour of arrivals is one block, so a block's worth of holds reaches a segment.
                daily_arrivals: RECORDS_PER_BLOCK as u64 * 24,
                worst_survivor_share: 0.5,
                retention_days: 1,
                grace_days: 1,
                survives_flush_window: 0.5,
                flush_window_hours: 1,
                residency_hours: 1,
            },
            ..MemoryPendingConfig::default()
        }
    }

    fn create(tx_id: TxId) -> PendingCommand {
        PendingCommand::Apply {
            at: ApplyIndex(tx_id.raw() as u64),
            effect: PendingEffect::Create {
                tx_id,
                debit_account: AccountId(1),
                credit_account: AccountId(2),
                amount: 10,
                ledger: 1,
                budget: BudgetGroup::ABSENT,
            },
        }
    }

    /// A day that runs out reaches the port as notices. Driven through the real worker thread, because the
    /// engine offering a void and the worker handing it over are different things and only the second is
    /// what the sequencer ever sees.
    #[test]
    fn a_day_that_runs_out_arrives_as_notices_on_the_port() {
        let (days, day) = DaySource::manual(0);
        let mut engine = MemoryPending::start_with_days(config(), days, PendingStorage::memory())
            .expect("a test config");

        let holds = RECORDS_PER_BLOCK + 1;
        for id in 1..=holds {
            let mut command = create(TxId(id as u128));
            while engine.send(command).is_err() {
                command = create(TxId(id as u128));
            }
        }
        // A record's segment is the day it was **written**, and writing happens on the engine's thread. So
        // the day may not move until the writes have landed, or the records belong to the next day and this
        // test would be waiting for an expiry two days out.
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.traffic().carried_on < RECORDS_PER_BLOCK as u64 {
            assert!(
                Instant::now() < deadline,
                "the engine never wrote the holds"
            );
        }

        // **Nothing has run out yet, and nothing may be released before it has** — which is the edge that
        // matters, since deleting early refuses a resolution that was still entitled to arrive.
        //
        // Proving a negative needs a bound, and it must not be a duration: two hundred milliseconds of
        // asking was the bound here, and on a busy machine that is a handful of loops rather than a
        // handful of rounds — it would have passed without checking anything. The bound is rounds now.
        // The sweep runs at the top of every round, so a hold this day had expired would be offered in
        // the first one; two records appended after the day changed prove two rounds have run, and the
        // second is what carries a notice the first could have owed.
        day.store(1, Ordering::Relaxed);
        for id in 1..=2u128 {
            let mut command = create(TxId(holds as u128 + id));
            while engine.send(command).is_err() {
                command = create(TxId(holds as u128 + id));
            }
            let want = holds as u64 + id as u64;
            let deadline = Instant::now() + Duration::from_secs(5);
            while engine.traffic().appended < want {
                assert!(
                    Instant::now() < deadline,
                    "the engine never took the command that proves a round ran"
                );
                assert!(
                    engine.notices().is_none(),
                    "a hold was offered before its lifetime ran out"
                );
            }
        }
        assert!(
            engine.notices().is_none(),
            "a hold was offered before its lifetime ran out"
        );

        day.store(2, Ordering::Relaxed);
        let mut offered = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while offered < RECORDS_PER_BLOCK && Instant::now() < deadline {
            if let Some(PendingNotice::HoldExpired { void }) = engine.notices() {
                assert_eq!(void.kind(), Ok(TransferKind::VoidExpiry));
                offered += 1;
            }
        }
        assert_eq!(
            offered, RECORDS_PER_BLOCK,
            "the worker handed over {offered} of {RECORDS_PER_BLOCK} expired holds"
        );
    }

    /// A snapshot reaches a directory through the real worker, and the numbers reach the port.
    ///
    /// The stage's own behaviour is unit-tested in `snapshots.rs` against an engine driven by hand; what
    /// is only checkable here is the wiring — that a policy in the configuration and a directory in the
    /// storage meet on the worker's thread, and that what the stage did is readable from the other side of
    /// it. Every one of those is a place a working stage can be attached to nothing.
    #[test]
    fn a_snapshot_reaches_the_directory_and_its_numbers_reach_the_port() {
        let path = std::env::temp_dir().join(format!("ledger-worker-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let dest = OpenBacking::files(&path, 0, false).expect("the scratch directory opens");
        let mut engine = MemoryPending::start_with_days(
            MemoryPendingConfig {
                snapshot: SnapshotPolicy {
                    // One committed batch is enough, so the first applies start a dump.
                    every: 1,
                    ..SnapshotPolicy::default()
                },
                ..config()
            },
            DaySource::WallClock,
            PendingStorage {
                blocks: OpenBacking::Memory,
                snapshots: Some(dest),
            },
        )
        .expect("a test config");

        for id in 1..=RECORDS_PER_BLOCK {
            let mut command = create(TxId(id as u128));
            while engine.send(command).is_err() {
                command = create(TxId(id as u128));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.snapshots().written == 0 {
            assert!(
                Instant::now() < deadline,
                "the worker wrote no snapshot in five seconds ({:?})",
                engine.snapshots()
            );
        }
        assert!(
            path.join("pending.snapshot").exists(),
            "the port says a snapshot was written and the directory has none"
        );
        let _ = std::fs::remove_dir_all(&path);
    }
}
