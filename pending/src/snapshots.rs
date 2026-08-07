//! Where a snapshot goes, and what paces it there.
//!
//! `snapshot.rs` is the format; this is the destination. The two are apart because the bytes serve two
//! readers and only one of them is a disk — a follower too far behind receives the same stream over a wire
//! that does not exist yet (design notes §15).
//!
//! **One file, replaced whole.** A dump is written to a partial name, made durable, and only then renamed
//! over the current one, with the directory synced after so the name itself survives. So a crash at any
//! point leaves either the previous snapshot or the new one, and never a prefix of the new one wearing the
//! current one's name — which a reader could not tell from a complete stream, since the header says how
//! many records there are and a truncated file simply ends early.
//!
//! **The cadence is a log distance, not a duration, and that is what removes the clock.** What recovery
//! costs is the effects it replays, and what the log has to retain is the entries it keeps — both are
//! counted in log positions, so measuring the interval in them needs neither a wall clock (which steps
//! backwards) nor a monotonic one (which restarts at zero). A node applying nothing writes no snapshots,
//! and a node at ten times the rate writes them ten times as often, without either being configured for.
//!
//! **The throttle is bytes a round**, which is the same shape every other background path here takes.
//! What it buys is measured rather than argued: the dump's own work is nothing — 42.7GB of stream costs
//! the engine three to eight seconds (`cargo bench -p ledger-pending --bench snapshot`) — and 85 seconds
//! of a 500MB/s volume. So the throttle is pacing against a disk and against the worker's thread, not
//! against this code, and the one thing it trades is below.
//!
//! **A round is what the dump gets, so it yields to traffic without being told to.** The stage takes one
//! chunk per worker round and the worker's rounds go to commands first, so the same throttle writes
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

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ledger_base::ports::ApplyIndex;

use crate::engine::PendingEngine;
use crate::snapshot::{NotASnapshot, SnapshotReader, SnapshotWriter, RECORD};

/// The snapshot a restart reads. One name, because the log retention rule makes an older one useless: a
/// snapshot is only restorable while the log still holds everything after its coverage.
const CURRENT: &str = "pending.snapshot";
/// The one being written. A separate name is what makes the replacement atomic — see the module note.
const PARTIAL: &str = "pending.snapshot.part";

/// Read back in pieces for the same reason it is written in them: the whole is 42.7GB at the design's
/// size. Larger than the write chunk because nothing competes with a restore — there is no traffic yet.
const READ_CHUNK: usize = 1 << 20;

/// Where snapshots go: a directory, opened and checked before any thread owns it.
///
/// Opened by the caller for the same reason `open_directory` is (`files.rs`): a directory that cannot be
/// used is a configuration error, and a configuration error has to be refused at start-up where somebody
/// can be told rather than on a worker thread whose only way to report it would be to panic (rule 6).
///
/// **Its own directory, not the store's.** Whether the two share a volume is a provisioning decision and
/// the design puts the log and the snapshot together (§2.2); either way the throttle is required, so
/// nothing here depends on the answer. Taking two paths is what keeps the answer outside the code.
pub struct SnapshotDir {
    /// Open for its own sake: a rename is not durable until the directory it happened in is synced, and
    /// syncing a directory means having it open.
    dir: File,
    path: PathBuf,
}

impl SnapshotDir {
    pub fn open(path: &Path) -> Result<Self, std::io::Error> {
        std::fs::create_dir_all(path)?;
        let dir = File::open(path)?;
        Ok(Self {
            dir,
            path: path.to_path_buf(),
        })
    }

    fn current(&self) -> PathBuf {
        self.path.join(CURRENT)
    }

    fn partial(&self) -> PathBuf {
        self.path.join(PARTIAL)
    }

    /// Reads the current snapshot into `engine`, and answers whether there was one. A directory with no
    /// snapshot in it is the ordinary state of a node that has not written one yet, so it is `Ok(false)`
    /// rather than an error.
    ///
    /// **What this restores is the index, the group totals and the coverage — not a node.** The engine's
    /// `RecordLog` still has no position: it does not know which block to write next or which blocks each
    /// day owns, so an engine restored here answers lookups against the blocks that are there and must not
    /// be written to. Deriving those from the restored slots is the first half of the start-up reconcile,
    /// and it is deliberately not here — see `status.md`. Until it is, this has one caller and it is a
    /// test.
    pub fn read_into(&self, engine: &mut PendingEngine) -> Result<bool, NotASnapshot> {
        let mut file = match File::open(self.current()) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(NotASnapshot::Unreadable),
        };
        let mut reader = SnapshotReader::new();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let read = Self::fill(&mut file, &mut chunk)?;
            if read == 0 {
                break;
            }
            // A stream is a whole number of records by construction, so a tail that is not one is a file
            // that was truncated — refused rather than interpreted, like every other malformed stream.
            if !read.is_multiple_of(RECORD) {
                return Err(NotASnapshot::Malformed);
            }
            reader.take_chunk(&chunk[..read], engine.index_mut())?;
        }
        if !reader.is_complete() {
            return Err(NotASnapshot::Malformed);
        }
        let coverage = reader.coverage();
        engine.restore(reader.into_groups(), coverage);
        Ok(true)
    }

    /// As much of `into` as the file has. A short `read` is ordinary rather than an error, so the loop is
    /// what turns a stream of them into whole chunks.
    fn fill(file: &mut File, into: &mut [u8]) -> Result<usize, NotASnapshot> {
        let mut at = 0;
        while at < into.len() {
            match file.read(&mut into[at..]) {
                Ok(0) => break,
                Ok(read) => at += read,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(NotASnapshot::Unreadable),
            }
        }
        Ok(at)
    }
}

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
    /// Bytes of the stream one worker round writes — the throttle. It is a `pwrite` on the thread that
    /// answers lookups, which is the same thread and the same cost as a block write, and the closed
    /// decision on those is what says the budget is real.
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
/// syscall is amortised over more of them. But a chunk is written inside one worker round, so it is a stall
/// on the thread every lookup passes through, and while a dump runs the median goes 1.5ms at 4KB to 6.5ms
/// at 64KB against a baseline of 1.3ms. A small chunk that runs more of the time costs the median a little;
/// a large one that runs less of the time costs a percentile a lot, and a percentile is what the contract
/// names. Design notes §19 has both curves.
pub const DEFAULT_BYTES_PER_ROUND: usize = 4096;

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
    /// Bytes handed to the file, published ones and given-up ones alike — what the throttle actually cost
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
    dest: SnapshotDir,
    policy: SnapshotPolicy,
    chunk: Vec<u8>,
    inflight: Option<InFlight>,
    /// The position the next dump is measured from. Moved by an attempt rather than by a success, so a
    /// destination that keeps refusing backs off to the cadence instead of retrying every round — it is
    /// not a claim about what is on disk, and `stats.covered` is.
    next_from: ApplyIndex,
    stats: SnapshotStats,
}

struct InFlight {
    writer: SnapshotWriter,
    file: File,
    rounds: u64,
}

impl Snapshots {
    pub fn new(dest: SnapshotDir, policy: SnapshotPolicy) -> Self {
        Self {
            chunk: vec![0u8; policy.bytes_per_round.max(RECORD)],
            dest,
            policy,
            inflight: None,
            next_from: ApplyIndex::default(),
            stats: SnapshotStats::default(),
        }
    }

    pub fn stats(&self) -> SnapshotStats {
        self.stats
    }

    /// This round's share: a chunk of the dump in flight, or the start of one if the log has moved far
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
    pub fn abandon(&mut self, engine: &mut PendingEngine) {
        let Some(run) = self.inflight.take() else {
            return;
        };
        self.give_up(run, engine);
    }

    /// The file first, the shadow second. Opening is the fallible half and it changes nothing observable
    /// (rule 17); `begin_snapshot` is what the engine then has to live with until the dump ends.
    fn begin(&mut self, engine: &mut PendingEngine) -> bool {
        let opened = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(self.dest.partial());
        let Ok(file) = opened else {
            // Counted and backed off rather than retried: a directory that refuses one round refuses the
            // next, and a retry every round would be a syscall per round for as long as it stays broken.
            self.stats.abandoned += 1;
            self.next_from = engine.applied_through();
            return false;
        };
        self.inflight = Some(InFlight {
            writer: engine.begin_snapshot(),
            file,
            rounds: 0,
        });
        true
    }

    fn step(&mut self, engine: &mut PendingEngine) -> bool {
        let Some(mut run) = self.inflight.take() else {
            return false;
        };
        run.rounds += 1;
        let written = engine.next_snapshot_chunk(&mut run.writer, &mut self.chunk);
        if written > 0 {
            self.stats.bytes += written as u64;
            if run.file.write_all(&self.chunk[..written]).is_err() {
                self.give_up(run, engine);
                return true;
            }
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
        if run.writer.is_complete() {
            self.publish(run, engine);
            return true;
        }
        if written == 0 || shadow > self.policy.shadow_budget {
            self.give_up(run, engine);
            return true;
        }
        self.inflight = Some(run);
        true
    }

    /// Durable, then current, then the name durable too — one call, because two of the three succeeding is
    /// a snapshot that is not one (rule 16). The shadow is already gone: the writer completing is what
    /// ended it.
    fn publish(&mut self, run: InFlight, engine: &mut PendingEngine) {
        let coverage = run.writer.coverage();
        let rounds = run.rounds;
        let published = run.file.sync_all().is_ok()
            && std::fs::rename(self.dest.partial(), self.dest.current()).is_ok()
            && self.dest.dir.sync_all().is_ok();
        // Whatever happened, the next dump is measured from here: a destination that refused the rename
        // will refuse it again in a round's time, and the cadence is the right thing to wait for.
        self.next_from = engine.applied_through();
        if published {
            self.stats.written += 1;
            self.stats.last_rounds = rounds;
            self.stats.covered = coverage.raw();
            return;
        }
        self.stats.abandoned += 1;
        let _ = std::fs::remove_file(self.dest.partial());
    }

    /// Ends a dump that will not be published: the shadow goes, the partial file goes, and the cadence
    /// starts again from here. Nothing is lost but the work — the current snapshot is untouched, which is
    /// the whole reason a dump is written to a name of its own.
    fn give_up(&mut self, run: InFlight, engine: &mut PendingEngine) {
        drop(run.file);
        let _ = std::fs::remove_file(self.dest.partial());
        engine.abandon_snapshot();
        self.stats.abandoned += 1;
        self.next_from = engine.applied_through();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use ledger_base::ports::PendingEffect;
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::{MemoryStore, RECORDS_PER_BLOCK};

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

        /// Snapshots and blocks in directories of their own, which is what the two flags are: the
        /// destination is not the store's, so a test that put them together would be testing a layout
        /// no deployment has to have.
        fn dir(&self) -> SnapshotDir {
            SnapshotDir::open(&self.0.join("snapshots")).expect("the scratch directory opens")
        }

        /// A store over the same block files each time it is asked for, so a restored engine reads the
        /// blocks the first one wrote — which is the whole shape of a restart. An engine restored over
        /// an empty store would find every slot and read none of them.
        fn store(&self) -> Box<dyn crate::block::DurableStore> {
            let (dir, path) = crate::files::open_directory(&self.0.join("blocks"))
                .expect("the scratch directory opens");
            Box::new(crate::files::FileStore::new(dir, path, 32, 0, false))
        }

        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.0.join("snapshots"))
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

    /// Rounds until the stage stops having anything to do, so a test drives it the way the worker does.
    fn drive(snapshots: &mut Snapshots, engine: &mut PendingEngine, rounds: usize) {
        for _ in 0..rounds {
            if !snapshots.round(engine) {
                break;
            }
        }
    }

    /// A snapshot written to a directory is read back into another engine over the same blocks, and
    /// every hold it carried answers the same. The whole point of a destination, end to end and through
    /// real files on both sides — which is the shape of a restart rather than of a round trip in memory.
    #[test]
    fn a_snapshot_written_to_a_directory_restores_into_another_engine() {
        let scratch = Scratch::new();
        let slots = 1 << 12;
        // Residency of nothing, so a carried slot has to be read from the files the first engine wrote.
        let mut source = PendingEngine::sized(slots, 1, 0, scratch.store());
        let holds = RECORDS_PER_BLOCK as u64 * 3;
        fill(&mut source, holds);

        let mut snapshots = Snapshots::new(
            scratch.dir(),
            SnapshotPolicy {
                every: 1,
                // Several rounds' worth, so the pacing is exercised rather than stepped over.
                bytes_per_round: RECORD * 4,
                ..SnapshotPolicy::default()
            },
        );
        drive(&mut snapshots, &mut source, 100_000);
        let stats = snapshots.stats();
        assert_eq!(stats.written, 1, "the dump did not reach the current name");
        assert_eq!(stats.abandoned, 0);
        assert!(stats.last_rounds > 1, "the throttle wrote it in one round");
        assert_eq!(
            scratch.files(),
            vec![CURRENT.to_string()],
            "the partial outlived the rename"
        );

        let mut restored = PendingEngine::sized(slots, 1, 0, scratch.store());
        assert!(
            scratch
                .dir()
                .read_into(&mut restored)
                .expect("a snapshot this table can take"),
            "the directory had no snapshot in it"
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
                "hold {id} differs after a round trip through the directory"
            );
            carried += 1;
        }
        assert!(carried > 0, "the snapshot carried nothing");
        assert!(restored.counts_agree());
    }

    /// A directory with no snapshot in it is the ordinary state of a node that has not written one, so it
    /// answers "there was none" rather than failing.
    #[test]
    fn an_empty_directory_is_not_a_broken_snapshot() {
        let scratch = Scratch::new();
        let mut restored = engine(1 << 12);
        assert_eq!(
            scratch.dir().read_into(&mut restored),
            Ok(false),
            "an empty directory was read as a broken snapshot"
        );
    }

    /// **The reason a dump is written to a name of its own.** A partial file is not the current one, so a
    /// crash part way through leaves the previous snapshot readable — and the previous one here is no
    /// snapshot at all, which is the same claim at its edge.
    #[test]
    fn a_dump_that_never_finished_is_not_the_current_snapshot() {
        let scratch = Scratch::new();
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);

        let mut snapshots = Snapshots::new(
            scratch.dir(),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: RECORD,
                ..SnapshotPolicy::default()
            },
        );
        // A few rounds only, so the stream is well short of its end.
        drive(&mut snapshots, &mut source, 4);
        assert_eq!(snapshots.stats().written, 0, "the dump finished too soon");
        assert_eq!(
            scratch.files(),
            vec![PARTIAL.to_string()],
            "a dump in progress was already wearing the current name"
        );

        let mut restored = engine(1 << 12);
        assert_eq!(
            scratch.dir().read_into(&mut restored),
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

        let mut snapshots = Snapshots::new(
            scratch.dir(),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: RECORD,
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
            next += 1;
            let _ = source.write(create(next as u128), ApplyIndex(next));
            source.drain(usize::MAX);
        }
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
            scratch.files().is_empty(),
            "an abandoned dump left its partial behind: {:?}",
            scratch.files()
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

        let mut snapshots = Snapshots::new(
            scratch.dir(),
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
    #[test]
    fn a_dump_is_given_up_on_when_the_store_breaks_under_it() {
        let scratch = Scratch::new();
        let mut source = engine(1 << 12);
        fill(&mut source, RECORDS_PER_BLOCK as u64 * 3);

        let mut snapshots = Snapshots::new(
            scratch.dir(),
            SnapshotPolicy {
                every: 1,
                bytes_per_round: RECORD,
                ..SnapshotPolicy::default()
            },
        );
        drive(&mut snapshots, &mut source, 4);
        assert!(
            source.shadowed_buckets() > 0 || snapshots.inflight.is_some(),
            "there was no dump in flight to give up on"
        );

        snapshots.abandon(&mut source);
        assert_eq!(
            source.shadowed_buckets(),
            0,
            "the shadow outlived the dump the seal ended"
        );
        assert!(
            scratch.files().is_empty(),
            "the given-up dump left its partial behind: {:?}",
            scratch.files()
        );
        assert_eq!(snapshots.stats().written, 0);
        assert_eq!(snapshots.stats().abandoned, 1);

        // And a second call is not a second abandonment: there is nothing in flight, so it counts nothing.
        snapshots.abandon(&mut source);
        assert_eq!(snapshots.stats().abandoned, 1);
    }

    /// Bytes that are not a snapshot are refused rather than interpreted, and a directory holding one is
    /// the same refusal a stream is — the destination adds no leniency of its own.
    #[test]
    fn a_current_file_that_is_not_a_snapshot_is_refused() {
        let scratch = Scratch::new();
        let dir = scratch.dir();
        std::fs::write(dir.current(), [0u8; RECORD]).expect("the junk file writes");

        let mut restored = engine(1 << 12);
        assert_eq!(
            dir.read_into(&mut restored),
            Err(NotASnapshot::Unrecognised)
        );

        // And a stream cut short is malformed rather than a table half restored being called a restore.
        std::fs::write(dir.current(), [0u8; RECORD - 1]).expect("the short file writes");
        assert_eq!(dir.read_into(&mut restored), Err(NotASnapshot::Malformed));
    }
}
