//! Where a snapshot goes, and what paces it there.
//!
//! `snapshot.rs` is the format; this is the destination. The two are apart because the bytes serve two
//! readers and only one of them is a disk — a follower too far behind receives the same stream over a wire
//! that does not exist yet (design notes §15).
//!
//! **It goes through the store, which is the one path to a disk** (§20). A chunk is a whole block at an
//! offset, submitted and answered for exactly as a record block is, so the dump's writes are counted,
//! queued and bounded by the same thing everything else on that volume is. Before this they were a
//! `File::write_all` beside the store: invisible to every IO figure the tools print, sharing no queue with
//! the reads they compete with.
//!
//! **Its queue is its own, and the volume's is not.** Blocks and snapshots are two writers with two
//! backlogs, two handle spaces and two reactions to a full queue — the log stops applying so backpressure
//! reaches the client, and a dump simply waits. What they share is the device, when a deployment declares
//! that they do: then one `DurableStore` serves both, because IO into one disk has to be managed and
//! watched in one place whoever asked for it.
//!
//! **One file, replaced whole.** A dump is written to a partial name, made durable by a barrier, and only
//! then renamed over the current one — and the store's `rename` syncs the directory, because a name is not
//! durable until the directory holding it is. So a crash at any point leaves either the previous snapshot
//! or the new one, and never a prefix of the new one wearing the current one's name.
//!
//! **The rename waits for every completion, and that is rule 22 rather than caution.** While the chunk
//! write was a synchronous `write_all`, "the stream is written" and "the last chunk returned" were the same
//! moment; submitted, they are not. A rename issued between them would publish a prefix. So the dump has
//! phases — filling, sealing, publishing — and nothing destructive happens while a write of this dump is
//! still outstanding, including the removal of a partial that is being given up on.
//!
//! **The cadence is a log distance, not a duration, and that is what removes the clock.** What recovery
//! costs is the effects it replays, and what the log has to retain is the entries it keeps — both are
//! counted in log positions, so measuring the interval in them needs neither a wall clock (which steps
//! backwards) nor a monotonic one (which restarts at zero). A node applying nothing writes no snapshots,
//! and a node at ten times the rate writes them ten times as often, without either being configured for.
//!
//! **The throttle is bytes a round**, which is the same shape every other background path here takes, and
//! it is now a whole number of blocks because a block is what the store takes. What it buys is measured
//! rather than argued: the dump's own work is nothing — 42.7GB of stream costs the engine three to eight
//! seconds (`cargo bench -p ledger-pending --bench snapshot`) — and 85 seconds of a 500MB/s volume. So the
//! throttle is pacing against a disk and against the worker's thread, not against this code.
//!
//! **A round is what the dump gets, so it yields to traffic without being told to.** The stage takes one
//! turn per worker round and the worker's rounds go to commands first, so the same throttle writes
//! 558MB/s when the engine has rounds to spare and 35MB/s when it is saturated — measured at 4KB a round,
//! five seconds of `partial-settle` at 1M/s against the same at the ceiling. That is the property a rate
//! limit expressed in bytes a second would not have had.
//!
//! **A longer dump costs memory, and that is the whole trade.** The stable read shadows every bucket
//! written ahead of its cursor, so the side buffer grows with how long the dump takes. That growth had no
//! ceiling, which is rule 20's shape exactly: a bound enforced by whichever structure ran out first. It
//! has one now — `shadow_budget`, declared in buckets — and exceeding it abandons the dump rather than
//! growing past it. Abandoning costs the work and nothing else: the previous snapshot is still current,
//! and the next cadence tries again.

use ledger_base::ports::ApplyIndex;

use crate::block::{Block, DurableStore, IoOwner, ObjectId, StoreFault, BLOCK_BYTES};
use crate::engine::PendingEngine;
use crate::snapshot::{NotASnapshot, SnapshotReader, SnapshotWriter};

/// How often a snapshot is written, how fast, and how much the stable read may hold aside while it is.
///
/// Grouped rather than three fields beside the rest of the engine's configuration (rule 11), because the
/// three only mean anything together: the cadence and the throttle between them decide the dump's
/// duration, and the duration is what the shadow budget has to cover.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotPolicy {
    /// Log positions between one dump's coverage and the next dump's start. **Zero writes none**, which is
    /// the default everywhere: a node that wrote files nobody asked for would change what a run means.
    pub every: u64,
    /// Bytes of the stream one worker round writes — the throttle. A whole number of blocks, because a
    /// block is the unit the store takes; `MemoryPendingConfig::validate` refuses anything else rather
    /// than rounding a declared number into a different one.
    pub bytes_per_round: usize,
    /// Buckets the copy-on-write side buffer may hold before the dump is abandoned. The declared ceiling
    /// rule 20 asks for: without it the bound is whatever the allocator does when a dump runs long.
    pub shadow_budget: usize,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            every: 0,
            bytes_per_round: DEFAULT_BYTES_PER_ROUND,
            shadow_budget: DEFAULT_SHADOW_BUDGET,
        }
    }
}

/// One round's write: a block's worth, which is 128 buckets.
///
/// **Chosen for the tail rather than for the total**, and the two disagree. Per byte written a larger chunk
/// is cheaper — 64KB costs 0.11% of throughput for each MB/s it writes against 4KB's 0.28%, because the
/// syscall is amortised over more of them. But a chunk was written inside one worker round, so it was a
/// stall on the thread every lookup passes through, and while a dump runs the median goes 1.5ms at 4KB to
/// 6.5ms at 64KB against a baseline of 1.3ms. A small chunk that runs more of the time costs the median a
/// little; a large one that runs less of the time costs a percentile a lot, and a percentile is what the
/// contract names. Design notes §19 has both curves.
///
/// **That argument is against an arrangement that has since changed, and the number has to be taken
/// again.** The chunk holds the worker's thread only while the write is synchronous; on a write lane it is
/// a queue's cost, and the cheaper large chunk wins. It stays at a block because that is the right number
/// for the default path, which is still the synchronous one — see §20's list of what it invalidates.
pub const DEFAULT_BYTES_PER_ROUND: usize = BLOCK_BYTES;

/// Buckets the shadow may hold: 64MB of slots, and it is headroom rather than a prediction.
///
/// What has to fit is index writes times the dump's duration — the standing population peaks around a
/// quarter of the product, since a bucket is dropped as the cursor reaches it. Measured under continuous
/// dumping at saturation it is 63,863 here and 19,951 at a tenth of that rate, so this is thirty-odd times
/// what this machine reaches. A deployment reads its own from a run rather than trusting this one: the
/// product's two terms are both a deployment's, and a run reports the peak beside the ceiling for exactly
/// that reason.
pub const DEFAULT_SHADOW_BUDGET: usize = 1 << 21;

/// What the worker has done about snapshots, across the thread boundary.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotStats {
    /// Dumps that reached the current name.
    pub written: u64,
    /// Dumps given up on: a write that failed, a shadow past its budget, or a store that broke under one.
    pub abandoned: u64,
    /// Bytes handed to the store, published ones and given-up ones alike — what the throttle actually cost
    /// the volume.
    pub bytes: u64,
    /// The most buckets the stable read held aside at once, against `shadow_budget`.
    pub shadow_peak: u64,
    /// Worker rounds the last published dump took. The duration, in the only unit the worker has that
    /// needs no clock.
    pub last_rounds: u64,
    /// Coverage of the last published dump: the position a restart would replay from.
    pub covered: u64,
}

/// The snapshot stage: where dumps go, how they are paced, and the one in flight.
///
/// State in one struct rather than fields scattered across the worker (rule 11) — and it owns the chunk
/// buffer for the same reason the engine owns its scratch block: a background path that allocated per
/// round would allocate for ever.
pub struct Snapshots {
    /// The volume this dump goes to when it is not the engine's own. `None` means the deployment declared
    /// one disk for both, so the blocks' store serves this too — one queue, because there is one device.
    own: Option<Box<dyn DurableStore>>,
    policy: SnapshotPolicy,
    /// One block, produced and then submitted. It is held across rounds when the queue would not take it:
    /// producing a chunk consumes the shadow entries for the buckets it read, so a chunk cannot be
    /// produced twice, and a refusal must therefore keep the bytes rather than the position.
    chunk: Box<Block>,
    /// Whether `chunk` holds a block that has been produced and not yet taken by the store.
    unsubmitted: bool,
    inflight: Option<InFlight>,
    /// Sequence numbers for this writer's submissions. Its own counter, tagged `IoOwner::Snapshot`, so a
    /// shared volume can hand each completion back to the writer that asked for it.
    handles: u64,
    /// Blocks of the volume's queue this dump may hold at once.
    ///
    /// **The blocks' priority is declared here rather than left to the order of two lines.** Within a
    /// round the drain submits before this stage does, so the dump already takes only what the blocks did
    /// not want — but a slot it takes it holds until the device answers, and on a device that has stalled
    /// while the ledger happens to have nothing to write, a chunk a round is enough for the dump to end up
    /// holding the whole queue. The blocks then wait for a dump's completions, applies stop at the
    /// buffer's ceiling and backpressure reaches a client, for a background job. Rule 18: the invariant
    /// held by a coincidence of how the pieces behave, so it is decided once and in one place.
    share: usize,
    /// The position the next dump is measured from. Moved by an attempt rather than by a success, so a
    /// destination that keeps refusing backs off to the cadence instead of retrying every round — it is
    /// not a claim about what is on disk, and `stats.covered` is.
    next_from: ApplyIndex,
    stats: SnapshotStats,
}

/// What a dump is doing. The phases exist because the write is submitted rather than done: each one is a
/// thing that must not happen until the one before it has been answered for.
enum Phase {
    /// Producing chunks and submitting them.
    Filling,
    /// The stream is submitted. A barrier is what makes it durable before the name changes; `None` is one
    /// the queue has not taken yet.
    Sealing(Option<u64>),
    /// The barrier has been answered, so the bytes are on the disk. What is left is the name — and it
    /// waits for the last completion, because a rename with a write of this dump still out would publish
    /// a prefix.
    Publishing,
    /// This dump will not be published. The shadow is already gone; what is left is to answer for the
    /// writes still out and then remove the partial — removing it first would leave writes landing in a
    /// file nothing will ever look at.
    Draining,
}

struct InFlight {
    writer: SnapshotWriter,
    rounds: u64,
    /// Blocks of the stream submitted so far, which is both the next offset and whether the next write is
    /// the one that brings the object into being.
    blocks: u64,
    /// Writes and barriers of this dump the store has not answered for.
    outstanding: usize,
    phase: Phase,
}

impl Snapshots {
    /// `own` is the volume this writes to, and `None` says the deployment declared the blocks' disk for
    /// both. Handed in rather than opened here: which volume a directory is on is a declaration, and the
    /// one thing this stage must not do is decide it (§20).
    pub fn new(own: Option<Box<dyn DurableStore>>, policy: SnapshotPolicy, share: usize) -> Self {
        Self {
            own,
            policy,
            chunk: Block::zeroed(),
            unsubmitted: false,
            inflight: None,
            handles: 0,
            share: share.max(1),
            next_from: ApplyIndex::default(),
            stats: SnapshotStats::default(),
        }
    }

    pub fn stats(&self) -> SnapshotStats {
        self.stats
    }

    /// The disk. One place, whether it is this stage's own or shared with the blocks — which is the whole
    /// of what "one path to a device" buys: nothing above here has two ways to reach one.
    fn volume<'a>(
        own: &'a mut Option<Box<dyn DurableStore>>,
        engine: &'a mut PendingEngine,
    ) -> &'a mut dyn DurableStore {
        match own {
            Some(store) => store.as_mut(),
            None => engine.volume(),
        }
    }

    /// The next completion of this stage's. On a volume of its own that is the store's queue; on a shared
    /// one the log is the poller and this is where it leaves what is not its own.
    fn completion(
        own: &mut Option<Box<dyn DurableStore>>,
        engine: &mut PendingEngine,
    ) -> Option<(u64, Result<(), StoreFault>)> {
        match own {
            Some(store) => store.poll_written(),
            None => engine.take_foreign_completion(),
        }
    }

    /// This round's share: a turn at the dump in flight, or the start of one if the log has moved far
    /// enough since the last. Answers whether it did anything, so the worker's backoff sees it as work.
    pub fn round(&mut self, engine: &mut PendingEngine) -> bool {
        if self.inflight.is_some() {
            return self.step(engine);
        }
        if self.policy.every == 0 {
            return false;
        }
        if engine.applied_through().raw() < self.next_from.raw().saturating_add(self.policy.every) {
            return false;
        }
        self.begin(engine)
    }

    /// Ends a dump in flight without publishing it. The worker calls this when the store has broken: the
    /// apply path is sealed, so nothing more will be applied and a half-written snapshot of a node that
    /// has stopped is work nobody will read.
    ///
    /// The shadow goes now; the partial goes when the writes still out have been answered for. Those are
    /// two events because the store made them two — see `give_up`.
    pub fn abandon(&mut self, engine: &mut PendingEngine) {
        let Some(mut run) = self.inflight.take() else {
            return;
        };
        self.give_up(&mut run, engine);
        self.inflight = Some(run);
    }

    /// Starts one. The partial is removed first so the dump's first chunk can bring the object into being:
    /// `creating` is what a store checks with `O_EXCL`, and a partial left by a crash would otherwise
    /// refuse the first write of every dump until one of them cleared it.
    fn begin(&mut self, engine: &mut PendingEngine) -> bool {
        let store = Self::volume(&mut self.own, engine);
        let _ = store.remove(ObjectId::SNAPSHOT_PARTIAL);
        self.inflight = Some(InFlight {
            writer: engine.begin_snapshot(),
            rounds: 0,
            blocks: 0,
            outstanding: 0,
            phase: Phase::Filling,
        });
        self.unsubmitted = false;
        true
    }

    fn step(&mut self, engine: &mut PendingEngine) -> bool {
        let Some(mut run) = self.inflight.take() else {
            return false;
        };
        run.rounds += 1;
        let mut broken = false;
        while let Some((handle, outcome)) = Self::completion(&mut self.own, engine) {
            debug_assert!(
                IoOwner::Snapshot.owns(handle),
                "a completion that is not this writer's reached it"
            );
            run.outstanding = run.outstanding.saturating_sub(1);
            if outcome.is_err() {
                broken = true;
            } else if matches!(run.phase, Phase::Sealing(Some(barrier)) if barrier == handle) {
                run.phase = Phase::Publishing;
            }
        }
        if broken {
            // A failed write ends the dump rather than retrying the chunk, and the shadow is why:
            // producing a chunk consumes the shadow entries for the buckets it read, so a chunk that was
            // produced and not written cannot be produced again. Retry is at the granularity of a dump,
            // which is the granularity the cadence already has.
            self.give_up(&mut run, engine);
        }
        match run.phase {
            Phase::Filling => self.fill(&mut run, engine),
            Phase::Sealing(None) => {
                // Everything this dump wrote is submitted, and a barrier is what makes it durable. Asked
                // for before the writes are answered for on purpose: the lane keeps the order, so a barrier
                // behind them in the queue is a barrier behind them on the device.
                let handle = IoOwner::Snapshot.handle(self.handles + 1);
                let store = Self::volume(&mut self.own, engine);
                if store.submit_barrier(handle) {
                    self.handles += 1;
                    run.outstanding += 1;
                    run.phase = Phase::Sealing(Some(handle));
                }
            }
            // Waiting: for the barrier, or for the last completion the two arms below need.
            Phase::Sealing(Some(_)) | Phase::Publishing | Phase::Draining => {}
        }
        // Asked after the chunk rather than before it, because the chunk is what the shadow was holding
        // for: the buckets this round read are dropped as they are read, so the peak is what is left.
        //
        // Sampled here and nowhere else, so what is really bounded is the budget plus whatever one round
        // shadows — a dump in flight takes a round every worker round, so that overshoot is one round's
        // writes. The alternative is a check inside the one method that writes a slot, which would put a
        // comparison on the apply path to bound a background one.
        let shadow = engine.shadowed_buckets();
        self.stats.shadow_peak = self.stats.shadow_peak.max(shadow as u64);
        if shadow > self.policy.shadow_budget {
            self.give_up(&mut run, engine);
        }
        // Nothing destructive while a write of this dump is still out, and the two ends of that need
        // different amounts of it. The **rename** is ordered by the barrier already — a barrier follows
        // every write it covers, and its completion is what `Publishing` means — so the count is belt
        // beside braces there. The **removal** has no barrier and nothing else: `abandon` arrives whenever
        // the store breaks, and removing the object with chunks still queued would leave them landing in a
        // file nothing will look at, their completions arriving for a dump that no longer exists. One
        // counter for both, because "this dump still has IO out" is one fact (rule 18).
        if run.outstanding > 0 {
            self.inflight = Some(run);
            return true;
        }
        match run.phase {
            Phase::Publishing => self.publish(run, engine),
            Phase::Draining => {
                let store = Self::volume(&mut self.own, engine);
                let _ = store.remove(ObjectId::SNAPSHOT_PARTIAL);
            }
            _ => self.inflight = Some(run),
        }
        true
    }

    /// Produces blocks and submits them, up to the throttle. One at a time, because a produced block that
    /// the queue would not take has to wait as bytes: the shadow entries it read are already gone, so the
    /// same block cannot be produced a second time.
    fn fill(&mut self, run: &mut InFlight, engine: &mut PendingEngine) {
        for _ in 0..(self.policy.bytes_per_round / BLOCK_BYTES).max(1) {
            // The share, before the throttle and before the queue: what this stage may hold of the
            // volume is its own bound, not whatever the queue happens to have free when it asks.
            if run.outstanding >= self.share {
                return;
            }
            if !self.unsubmitted {
                let produced = engine.next_snapshot_chunk(&mut run.writer, &mut self.chunk[..]);
                if produced == 0 {
                    run.phase = Phase::Sealing(None);
                    return;
                }
                // The last chunk of a stream is short of a block, and a disk takes whole ones. Zeroed
                // rather than left as whatever the buffer held, because the reader has to be able to tell
                // padding from a record it was not promised — see `SnapshotReader::take_chunk`.
                self.chunk[produced..].fill(0);
                self.unsubmitted = true;
            }
            let handle = IoOwner::Snapshot.handle(self.handles + 1);
            let creating = run.blocks == 0;
            let offset = run.blocks * BLOCK_BYTES as u64;
            let store = Self::volume(&mut self.own, engine);
            // Refused is the volume's queue being full, which for this writer means waiting — the log's
            // answer to the same refusal is to stop applying, and one queue cannot express two reactions.
            if !store.submit_write(
                handle,
                ObjectId::SNAPSHOT_PARTIAL,
                offset,
                &self.chunk,
                creating,
            ) {
                return;
            }
            self.handles += 1;
            self.unsubmitted = false;
            run.blocks += 1;
            run.outstanding += 1;
            self.stats.bytes += BLOCK_BYTES as u64;
            if run.writer.is_complete() {
                run.phase = Phase::Sealing(None);
                return;
            }
        }
    }

    /// The name, and then the cadence. The bytes are durable already — the barrier this waited for is what
    /// made them so — and the store's rename is what makes the *name* durable, which is the half a `sync`
    /// of the file alone would leave out.
    fn publish(&mut self, run: InFlight, engine: &mut PendingEngine) {
        let coverage = run.writer.coverage();
        let store = Self::volume(&mut self.own, engine);
        let published = store
            .rename(ObjectId::SNAPSHOT_PARTIAL, ObjectId::SNAPSHOT_CURRENT)
            .is_ok();
        if !published {
            let _ = store.remove(ObjectId::SNAPSHOT_PARTIAL);
        }
        // Whatever happened, the next dump is measured from here: a destination that refused the rename
        // will refuse it again in a round's time, and the cadence is the right thing to wait for.
        self.next_from = engine.applied_through();
        if published {
            self.stats.written += 1;
            self.stats.last_rounds = run.rounds;
            self.stats.covered = coverage.raw();
            return;
        }
        self.stats.abandoned += 1;
    }

    /// Ends a dump that will not be published: the shadow goes now, the cadence starts again from here, and
    /// the partial waits for the writes still out. Nothing is lost but the work — the current snapshot is
    /// untouched, which is the whole reason a dump is written to a name of its own.
    ///
    /// The shadow does not wait for the IO and must not: it is the index holding buckets aside, so keeping
    /// it until a device answers would make a slow disk cost the apply path memory.
    fn give_up(&mut self, run: &mut InFlight, engine: &mut PendingEngine) {
        if matches!(run.phase, Phase::Draining) {
            return;
        }
        run.phase = Phase::Draining;
        self.unsubmitted = false;
        engine.abandon_snapshot();
        self.stats.abandoned += 1;
        self.next_from = engine.applied_through();
    }

    /// Reads the current snapshot into `engine`, and answers whether there was one. A volume with no
    /// snapshot on it is the ordinary state of a node that has not written one yet, so it is `Ok(false)`
    /// rather than an error — and that is a question for `exists` rather than for a read, because a read of
    /// block zero cannot tell an absent object from an absent block.
    ///
    /// **What this restores is the index, the group totals and the coverage — not a node.** The engine's
    /// `RecordLog` still has no position: it does not know which block to write next or which blocks each
    /// day owns, so an engine restored here answers lookups against the blocks that are there and must not
    /// be written to. Deriving those from the restored slots is the first half of the start-up reconcile,
    /// and it is deliberately not here — see `status.md`. Until it is, this has one caller and it is a
    /// test.
    pub fn read_into(&mut self, engine: &mut PendingEngine) -> Result<bool, NotASnapshot> {
        let store = Self::volume(&mut self.own, engine);
        if !store.exists(ObjectId::SNAPSHOT_CURRENT) {
            return Ok(false);
        }
        let mut reader = SnapshotReader::new();
        let mut at = 0u64;
        while !reader.is_complete() {
            let store = Self::volume(&mut self.own, engine);
            // The two faults say two different things here and the difference is worth keeping. A block
            // this object does not have, before the header's count has been met, is a stream that ends
            // earlier than it said it would — malformed. A device that refused is not a claim about the
            // format at all (§17).
            match store.read_at(
                ObjectId::SNAPSHOT_CURRENT,
                at * BLOCK_BYTES as u64,
                &mut self.chunk,
            ) {
                Ok(()) => {}
                Err(StoreFault::Missing) => return Err(NotASnapshot::Malformed),
                Err(StoreFault::Device) => return Err(NotASnapshot::Unreadable),
            }
            at += 1;
            reader.take_chunk(&self.chunk, engine.index_mut())?;
        }
        let coverage = reader.coverage();
        engine.restore(reader.into_groups(), coverage);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ledger_base::ports::PendingEffect;
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::{MemoryStore, RECORDS_PER_BLOCK};
    use crate::snapshot::RECORD;
    use crate::testkit::HoldingStore;

    /// A directory of its own per test, removed with it. Named from the process and a counter for the same
    /// reason `files.rs`'s is: a test that could collide with another run of itself fails for a reason
    /// nobody will find.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ledger-snapshots-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        fn volume(&self, at: &str) -> Box<dyn DurableStore> {
            let (dir, path) = crate::files::open_directory(&self.0.join(at))
                .expect("the scratch directory opens");
            Box::new(crate::files::FileStore::new(dir, path, 32, 0, false))
        }

        /// Snapshots and blocks on volumes of their own, which is what two directories mean until a
        /// deployment declares otherwise.
        fn snapshots(&self) -> Box<dyn DurableStore> {
            self.volume("snapshots")
        }

        fn files(&self, at: &str) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.0.join(at))
                .expect("the scratch directory reads")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }

        fn write_current(&self, bytes: &[u8]) {
            let dir = self.0.join("snapshots");
            std::fs::create_dir_all(&dir).expect("the scratch directory");
            std::fs::write(dir.join("pending.snapshot"), bytes).expect("the junk file writes");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn create(id: u128) -> PendingEffect {
        PendingEffect::Create {
            tx_id: TxId(id),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 100,
            ledger: 1,
            budget: BudgetGroup(7),
        }
    }

    fn engine(slots: usize) -> PendingEngine {
        PendingEngine::sized(slots, 1, 1024, Box::new(MemoryStore::default()))
    }

    /// A stage on a volume of its own, which is what two directories mean. The share is the whole of the
    /// test store's queue unless a test says otherwise: what these cover is the dump, not the sharing.
    fn apart(store: Box<dyn DurableStore>, policy: SnapshotPolicy) -> Snapshots {
        Snapshots::new(Some(store), policy, 32)
    }

    fn fill(engine: &mut PendingEngine, holds: u64) {
        for id in 1..=holds {
            engine
                .write(create(id as u128), ApplyIndex(id))
                .expect("the index took the hold");
            engine.drain(usize::MAX);
        }
        engine.sync();
        engine.collect_writes();
    }

    /// Rounds until the stage stops having anything to do, so a test drives it the way the worker does —
    /// the log's own poll included, because on a shared volume that is what routes this stage's
    /// completions to it.
    fn drive(snapshots: &mut Snapshots, engine: &mut PendingEngine, rounds: usize) {
        for _ in 0..rounds {
            let worked = snapshots.round(engine);
            engine.collect_writes();
            if !worked {
                break;
            }
        }
    }

    /// A snapshot written to a volume is read back into another engine over the same blocks, and every
    /// hold it carried answers the same. The whole point of a destination, end to end and through real
    /// files on both sides — which is the shape of a restart rather than of a round trip in memory.
    #[test]
    fn a_snapshot_written_to_a_volume_restores_into_another_engine() {
        let scratch = Scratch::new();
        let slots = 1 << 12;
        // Residency of nothing, so a carried slot has to be read from the files the first engine wrote.
        let mut source = PendingEngine::sized(slots, 1, 0, scratch.volume("blocks"));
        let holds = RECORDS_PER_BLOCK as u64 * 3;
        fill(&mut source, holds);

        let mut snapshots = apart(
            scratch.snapshots(),
            SnapshotPolicy {
                every: 1,
                // One block a round, so several rounds are needed and the pacing is exercised rather than
                // stepped over.
                bytes_per_round: BLOCK_BYTES,
                ..SnapshotPolicy::default()
            },
        );
        drive(&mut snapshots, &mut source, 100_000);
        let stats = snapshots.stats();
        assert_eq!(stats.written, 1, "the dump did not reach the current name");
        assert_eq!(stats.abandoned, 0);
        assert!(stats.last_rounds > 1, "the throttle wrote it in one round");
        assert_eq!(
            scratch.files("snapshots"),
            vec!["pending.snapshot".to_string()],
            "the partial outlived the rename"
        );

        let mut restored = PendingEngine::sized(slots, 1, 0, scratch.volume("blocks"));
        assert!(
            apart(scratch.snapshots(), SnapshotPolicy::default())
                .read_into(&mut restored)
                .expect("a snapshot this table can take"),
            "the volume had no snapshot on it"
        );
        assert_eq!(restored.coverage(), ApplyIndex(stats.covered));

        // Only the holds whose blocks were durable are carried — the ones still in the writeback buffer
        // are deliberately left out, because the log has them. For the rest the two answers have to
        // agree field for field, the group's totals included, since those ride on the record.
        let mut carried = 0;
        for id in 1..=holds {
            let Some(after) = restored.lookup(TxId(id as u128)) else {
                continue;
            };
            let before = source
                .lookup(TxId(id as u128))
                .expect("a hold the snapshot carried is one the engine still has");
            assert_eq!(
                (
                    before.remaining,
                    before.budget_members,
                    before.debit_account
                ),
                (after.remaining, after.budget_members, after.debit_account),
                "hold {id} differs after a round trip through the volume"
            );
            carried += 1;
        }
        assert!(carried > 0, "the snapshot carried nothing");
        assert!(restored.counts_agree());
    }

    /// **One disk, one store, and the log is what polls it.** Two directories are two volumes and a
    /// declaration is what collapses them; when it does, the dump submits to the blocks' store and its
    /// completions come back through the log's own poll. Both halves are asserted: the snapshot and the
    /// day's blocks end up in one directory, and the dump publishes — which it cannot do unless every
    /// completion reached the writer that was waiting for it.
    #[test]
    fn a_declared_volume_carries_the_blocks_and_the_snapshot_on_one_store() {
        let scratch = Scratch::new();
        let slots = 1 << 12;
        let mut source = PendingEngine::sized(slots, 1, 0, scratch.volume("one"));
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);

        // No store of its own: the blocks' volume is this dump's volume too.
        let mut snapshots = Snapshots::new(
            None,
            SnapshotPolicy {
                every: 1,
                bytes_per_round: BLOCK_BYTES,
                ..SnapshotPolicy::default()
            },
            32,
        );
        drive(&mut snapshots, &mut source, 100_000);
        assert_eq!(
            snapshots.stats().written,
            1,
            "a dump on the blocks' own volume never published, so its completions did not reach it"
        );
        let names = scratch.files("one");
        assert!(
            names.contains(&"pending.snapshot".to_string())
                && names.iter().any(|name| name.starts_with("seg-")),
            "one volume did not hold both the blocks and the snapshot: {names:?}"
        );

        let mut restored = PendingEngine::sized(slots, 1, 0, scratch.volume("one"));
        assert!(
            Snapshots::new(None, SnapshotPolicy::default(), 32)
                .read_into(&mut restored)
                .expect("a snapshot this table can take"),
            "the shared volume had no snapshot on it"
        );
        assert!(restored.counts_agree());
    }

    /// **The name waits for the barrier**, and this is the test that says so. While the write was a
    /// synchronous call, "the last chunk returned" and "the stream is on the disk" were one event;
    /// submitted, they are two, and a rename between them publishes a prefix (rule 22).
    ///
    /// The store here answers nothing until it is told to, which is what a device with a queue does. So
    /// the dump reaches the end of its stream with its completions outstanding, and the current name may
    /// not appear until they arrive.
    #[test]
    fn a_dump_publishes_nothing_until_its_writes_are_answered_for() {
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);
        let held = HoldingStore::default();
        let mut snapshots = apart(
            Box::new(held.clone()),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: BLOCK_BYTES,
                ..SnapshotPolicy::default()
            },
        );

        drive(&mut snapshots, &mut source, 10_000);
        assert!(
            held.holds(ObjectId::SNAPSHOT_PARTIAL),
            "the dump submitted nothing to hold"
        );
        assert_eq!(
            snapshots.stats().written,
            0,
            "a dump was published while its writes were still outstanding"
        );
        assert!(
            !held.holds(ObjectId::SNAPSHOT_CURRENT),
            "the current name appeared before the stream was on the disk"
        );

        // And it publishes once they are answered for, which is the same claim from the other side: what
        // it was waiting for was the completions and nothing else.
        held.stop_holding();
        drive(&mut snapshots, &mut source, 10_000);
        assert_eq!(snapshots.stats().written, 1);
        assert!(held.holds(ObjectId::SNAPSHOT_CURRENT));
    }

    /// **A dump may hold only its declared share of the volume's queue.** Within a round the blocks
    /// already ask first, so the dump takes what they did not want — but a slot it takes it holds until
    /// the device answers, and a device that has stalled while the ledger has nothing to write would
    /// otherwise let a chunk a round grow into the whole queue. The blocks would then wait on a
    /// background job's completions, and a client would be refused for a snapshot (rule 18).
    ///
    /// The store here answers nothing, which is that stalled device, and the throttle asks for four times
    /// the share so the bound is the share rather than the throttle.
    #[test]
    fn a_dump_holds_only_its_share_of_the_volume() {
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);
        let held = HoldingStore::default();
        let share = 2;
        let mut snapshots = Snapshots::new(
            Some(Box::new(held.clone())),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: BLOCK_BYTES * 8,
                ..SnapshotPolicy::default()
            },
            share,
        );

        for _ in 0..64 {
            snapshots.round(&mut source);
            assert!(
                held.writes_inflight() <= share,
                "the dump held {} of a {share}-block share",
                held.writes_inflight()
            );
        }
        assert_eq!(
            held.writes_inflight(),
            share,
            "the dump never reached its share, so the bound was never the thing being tested"
        );
    }

    /// A volume with no snapshot on it is the ordinary state of a node that has not written one, so it
    /// answers "there was none" rather than failing.
    #[test]
    fn a_volume_with_no_snapshot_on_it_is_not_a_broken_one() {
        let scratch = Scratch::new();
        let mut restored = engine(1 << 12);
        assert_eq!(
            apart(scratch.snapshots(), SnapshotPolicy::default()).read_into(&mut restored),
            Ok(false),
            "an empty volume was read as a broken snapshot"
        );
    }

    /// **The reason a dump is written to a name of its own.** A partial is not the current one, so a
    /// crash part way through leaves the previous snapshot readable — and the previous one here is no
    /// snapshot at all, which is the same claim at its edge.
    #[test]
    fn a_dump_that_never_finished_is_not_the_current_snapshot() {
        let scratch = Scratch::new();
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);

        let mut snapshots = apart(
            scratch.snapshots(),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: BLOCK_BYTES,
                ..SnapshotPolicy::default()
            },
        );
        // A few rounds only, so the stream is well short of its end.
        drive(&mut snapshots, &mut source, 3);
        assert_eq!(snapshots.stats().written, 0, "the dump finished too soon");
        assert_eq!(
            scratch.files("snapshots"),
            vec!["pending.snapshot.part".to_string()],
            "a dump in progress was already wearing the current name"
        );

        let mut restored = engine(1 << 12);
        assert_eq!(
            apart(scratch.snapshots(), SnapshotPolicy::default()).read_into(&mut restored),
            Ok(false),
            "a dump in progress was readable as the current snapshot"
        );
    }

    /// The shadow's declared ceiling, and what happens at it: the dump is given up on rather than the side
    /// buffer growing past what was declared for it (rule 20).
    ///
    /// The budget here is zero, so the first bucket written ahead of the cursor is over it — which is what
    /// makes this a test about the ceiling rather than about how much traffic it takes to reach one.
    #[test]
    fn a_shadow_past_its_budget_ends_the_dump_rather_than_growing() {
        let scratch = Scratch::new();
        let slots = 1 << 12;
        let mut source = engine(slots);
        let holds = RECORDS_PER_BLOCK as u64 * 3;
        fill(&mut source, holds);

        let mut snapshots = apart(
            scratch.snapshots(),
            SnapshotPolicy {
                // Far enough that the writes below cannot start a second dump: what is being asserted is
                // what became of the first one, and a fresh partial beside it would answer for it.
                every: holds,
                bytes_per_round: BLOCK_BYTES,
                shadow_budget: 0,
            },
        );
        // One round, then a write into the table between rounds — which is what shadows a bucket the dump
        // has not reached.
        let mut next = holds;
        for _ in 0..64 {
            if !snapshots.round(&mut source) {
                break;
            }
            source.collect_writes();
            next += 1;
            let _ = source.write(create(next as u128), ApplyIndex(next));
            source.drain(usize::MAX);
        }
        // And on to where the dump has answered for its writes, because the partial goes then and not
        // when the shadow does.
        drive(&mut snapshots, &mut source, 8);
        assert_eq!(
            snapshots.stats().written,
            0,
            "a dump past its shadow budget was published"
        );
        assert!(
            snapshots.stats().abandoned > 0,
            "the shadow grew past its budget without the dump being given up on"
        );
        assert_eq!(
            source.shadowed_buckets(),
            0,
            "the shadow outlived the dump it was for"
        );
        assert!(
            scratch.files("snapshots").is_empty(),
            "an abandoned dump left its partial behind: {:?}",
            scratch.files("snapshots")
        );
    }

    /// The cadence is a log distance: a node that applies nothing writes nothing, however many rounds it
    /// runs, and one that has moved far enough writes without being told to.
    #[test]
    fn the_cadence_follows_the_log_and_not_the_rounds() {
        let scratch = Scratch::new();
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64);
        let applied = source.applied_through().raw();

        let mut snapshots = apart(
            scratch.snapshots(),
            SnapshotPolicy {
                // Further than this engine has ever got, so no number of rounds reaches it.
                every: applied * 100,
                ..SnapshotPolicy::default()
            },
        );
        drive(&mut snapshots, &mut source, 1_000);
        assert_eq!(
            snapshots.stats().written,
            0,
            "a snapshot was written before the log had moved far enough"
        );

        // And the same stage writes one the moment the log passes the distance, with nothing else changed.
        let mut next = RECORDS_PER_BLOCK as u64;
        while source.applied_through().raw() < applied * 100 {
            next += 1;
            let _ = source.write(create(next as u128), ApplyIndex(next));
            source.drain(usize::MAX);
        }
        source.sync();
        source.collect_writes();
        drive(&mut snapshots, &mut source, 100_000);
        assert_eq!(
            snapshots.stats().written,
            1,
            "the log passed the distance and no snapshot followed"
        );
    }

    /// A broken store ends the dump in flight, which is what the worker does with it when the apply path
    /// seals: nothing more will be applied, so a dump of a state that has stopped moving is bytes nobody
    /// will read, and finishing it would hold the shadow for the whole of it.
    ///
    /// The shadow goes at once and the partial goes when the writes still out have been answered for —
    /// two events, because a removal that overtook a write would leave it landing in a file nothing looks
    /// at.
    #[test]
    fn a_dump_is_given_up_on_when_the_store_breaks_under_it() {
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);
        // Writes answered only when told, so the partial has something to wait for: what the seal ends is
        // the dump, and what ends the object is the last completion.
        let held = HoldingStore::default();
        let mut snapshots = apart(
            Box::new(held.clone()),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: BLOCK_BYTES,
                ..SnapshotPolicy::default()
            },
        );
        drive(&mut snapshots, &mut source, 3);
        assert!(
            held.holds(ObjectId::SNAPSHOT_PARTIAL),
            "there was no dump in flight to give up on"
        );

        snapshots.abandon(&mut source);
        assert_eq!(
            source.shadowed_buckets(),
            0,
            "the shadow outlived the dump the seal ended"
        );
        assert_eq!(snapshots.stats().written, 0);
        assert_eq!(snapshots.stats().abandoned, 1);

        // **The shadow goes now and the object goes later**, which is the whole of what a submitted write
        // changed here: removing it with chunks still queued would leave them landing in a file nothing
        // will look at.
        drive(&mut snapshots, &mut source, 8);
        assert!(
            held.holds(ObjectId::SNAPSHOT_PARTIAL),
            "the partial was removed while writes to it were still outstanding"
        );
        held.stop_holding();
        drive(&mut snapshots, &mut source, 8);
        assert!(
            !held.holds(ObjectId::SNAPSHOT_PARTIAL),
            "the given-up dump left its partial behind"
        );

        // And a second call is not a second abandonment: there is nothing in flight, so it counts nothing.
        snapshots.abandon(&mut source);
        assert_eq!(snapshots.stats().abandoned, 1);
    }

    /// Bytes that are not a snapshot are refused rather than interpreted, and an object holding them is
    /// the same refusal a stream is — the destination adds no leniency of its own.
    #[test]
    fn a_current_object_that_is_not_a_snapshot_is_refused() {
        let scratch = Scratch::new();
        let mut restored = engine(1 << 12);
        scratch.write_current(&[0u8; BLOCK_BYTES]);
        assert_eq!(
            apart(scratch.snapshots(), SnapshotPolicy::default()).read_into(&mut restored),
            Err(NotASnapshot::Unrecognised)
        );

        // And a stream cut short is malformed rather than a table half restored being called a restore.
        // Short of a whole block is what the store answers `Missing` to, which is the object ending
        // before its own header said it would.
        scratch.write_current(&[0u8; RECORD]);
        assert_eq!(
            apart(scratch.snapshots(), SnapshotPolicy::default()).read_into(&mut restored),
            Err(NotASnapshot::Malformed)
        );
    }
}
