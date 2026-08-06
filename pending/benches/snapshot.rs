//! What a snapshot costs to write and what recovery costs to replay — the two numbers design notes §15's
//! interval arithmetic waits on.
//!
//! The interval is decided by how long recovery may take and nothing else: a long interval is nearly free in
//! IO and a short one is not, so what a snapshot writes per hour matters far less than what a restart has to
//! replay. §15 could not choose one because the replay rate was unmeasured — there was no replay path. There
//! is now.
//!
//! Both rows extrapolate, and both say what they multiply. The dump is a sequential write of same-sized
//! records, so its rate should not move with the table's size; the replay is one apply per effect, on the same
//! path the ledger uses. Neither number includes a device: there is no disk under this, so what is measured is
//! the engine's own work and a real deployment adds its storage on top.

use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_base::ports::{ApplyIndex, PendingEffect};
use ledger_base::{AccountId, BudgetGroup, TxId};
use ledger_benchkit::{BenchOptions, Samples, STRIDE};
use ledger_pending::{MemoryStore, PendingEngine, SnapshotReader, LOAD_TARGET, SNAPSHOT_RECORD};

/// Slots the design's index has: 4.8 billion holds at the 0.90 load target, eight bytes each.
const DESIGN_SLOTS: u64 = 5_333_333_333;
const DESIGN_BYTES: u64 = DESIGN_SLOTS * 8;

/// A day's arrivals at the design's scale. What a recovery replays is this times the snapshot's age in days,
/// plus the writeback window — so it is the figure the interval is measured in.
const DESIGN_DAILY_EFFECTS: u64 = 300_000_000;

/// Chunk the dump is written in. A real one is paced by a declared number of buckets per round; this is one
/// buffer's worth, which is what the rate itself does not depend on.
const CHUNK: usize = 1 << 16;

fn key(ordinal: u64) -> TxId {
    TxId(u128::from(ordinal.wrapping_mul(STRIDE)) + 1)
}

fn create(ordinal: u64) -> PendingEffect {
    PendingEffect::Create {
        tx_id: key(ordinal),
        debit_account: AccountId(1),
        credit_account: AccountId(2),
        amount: 100,
        ledger: 1,
        budget: BudgetGroup::ABSENT,
    }
}

/// An engine holding `holds` holds in a table of `slots`, with a writeback window wide enough that most of
/// them reach a block — which is what a snapshot carries.
///
/// The slot count is the caller's because a snapshot only restores into a table of the same size: a recovery
/// row has to give the source room for the tail it will replay, or the restore it measures is a refusal.
fn filled(holds: u64, slots: usize) -> PendingEngine {
    let mut engine = PendingEngine::sized(slots, 8, 1 << 20, Box::new(MemoryStore::default()));
    for at in 1..=holds {
        let _ = engine.write(create(at), ApplyIndex(at));
    }
    engine
}

/// Writing the whole stream out. Reported in bytes a second, because that is what the interval trades against
/// the log's own write rate.
fn dump_rate(options: &BenchOptions) {
    println!();
    println!("writing a snapshot");
    for holds in [100_000u64, 400_000, 1_600_000] {
        let mut engine = filled(holds, (holds as f64 / LOAD_TARGET) as usize);
        let bytes = engine.begin_snapshot().bytes();
        engine.abandon_snapshot();
        let mut chunk = vec![0u8; CHUNK];
        let mut samples = Samples::new(
            format!(
                "dump  {holds:>9} holds ({:>6.1}MB, {} records)",
                bytes as f64 / 1e6,
                bytes / SNAPSHOT_RECORD as u64
            ),
            bytes / SNAPSHOT_RECORD as u64,
        );
        let mut elapsed = Vec::new();
        for _ in 0..options.repeat {
            let mut writer = engine.begin_snapshot();
            let started = Instant::now();
            let mut total = 0u64;
            loop {
                let written = engine.next_snapshot_chunk(&mut writer, &mut chunk);
                if written == 0 {
                    break;
                }
                total += written as u64;
            }
            let took = started.elapsed();
            black_box(total);
            assert_eq!(total, bytes, "the stream was not the size it said");
            elapsed.push(took);
            samples.add(took);
        }
        samples.report();
        let took = median(elapsed);
        let rate = bytes as f64 / took.as_secs_f64();
        // Engine time, not device time, and the difference decides which is the bottleneck: at a few
        // hundred MB/s a real volume takes minutes for the same bytes, so the throttle is pacing against the
        // disk rather than against this.
        println!(
            "    {:.0} MB/s of engine time, so the design's {:.1}GB costs it {:.0}s — a 500MB/s volume {:.0}s",
            rate / 1e6,
            DESIGN_BYTES as f64 / 1e9,
            DESIGN_BYTES as f64 / rate,
            DESIGN_BYTES as f64 / 500e6
        );
    }
}

/// Restoring a stream and then replaying the log's tail — the two halves of recovery, timed apart because
/// only the second grows with the snapshot's age.
fn recovery_rate(options: &BenchOptions) {
    println!();
    println!("restoring a snapshot, then replaying the log's tail");
    for holds in [100_000u64, 400_000] {
        // Room for the tail as well, so the restore below is into a table the stream describes.
        let slots = (holds as f64 * 2.0 / LOAD_TARGET) as usize;
        let mut engine = filled(holds, slots);
        // The tail is work that arrived *after* the snapshot, which is what a log holds past its coverage —
        // an hour of the writeback window plus however old the snapshot is. Taking it from the effects the
        // snapshot already has would measure the wrong path and only a few hundred of them: with a window of
        // eight blocks, coverage lags by eight blocks and no more.
        let tail: Vec<(PendingEffect, ApplyIndex)> = ((holds + 1)..=(holds * 2))
            .map(|at| (create(at), ApplyIndex(at)))
            .collect();
        let reflects = engine.applied_through();

        let mut restore_samples = Samples::new(
            format!("restore {holds:>8} holds"),
            engine.begin_snapshot().records(),
        );
        engine.abandon_snapshot();
        let mut replay_samples = Samples::new(
            format!("replay  {:>8} effects", tail.len()),
            tail.len() as u64,
        );
        let mut replay_elapsed = Vec::new();

        let mut chunk = vec![0u8; CHUNK];
        for _ in 0..options.repeat {
            let mut into =
                PendingEngine::sized(slots, 8, 1 << 20, Box::new(MemoryStore::default()));
            let mut writer = engine.begin_snapshot();
            let mut reader = SnapshotReader::new();
            let started = Instant::now();
            loop {
                let written = engine.next_snapshot_chunk(&mut writer, &mut chunk);
                if written == 0 {
                    break;
                }
                reader
                    .take_chunk(&chunk[..written], into.index_mut())
                    .expect("a stream this table can take");
            }
            restore_samples.add(started.elapsed());
            let covered = reader.coverage();
            into.restore(reader.into_groups(), covered);

            let started = Instant::now();
            for (effect, at) in &tail {
                let _ = into.replay(*effect, *at, reflects);
            }
            let took = started.elapsed();
            replay_elapsed.push(took);
            replay_samples.add(took);
        }
        restore_samples.report();
        replay_samples.report();

        let took = median(replay_elapsed);
        let per_effect = took.as_secs_f64() / tail.len() as f64;
        // The engine's own time, and on its own it invites the wrong conclusion: recovery also has to *read*
        // the log, and at 112 bytes an effect a day of it is 34GB. On a 500MB/s volume that read is the larger
        // half by a factor of six, so what bounds recovery is the log's bandwidth rather than the apply path —
        // which is the same answer the dump rows give, and it is what makes a long snapshot interval cheap.
        let log_bytes = DESIGN_DAILY_EFFECTS as f64 * 112.0;
        println!(
            "    {:.1}M effects/s, so a design day's {}M costs {:.0}s of engine time — against {:.0}s to \
             read the {:.0}GB of log it is in, at 500MB/s",
            1e-6 / per_effect,
            DESIGN_DAILY_EFFECTS / 1_000_000,
            per_effect * DESIGN_DAILY_EFFECTS as f64,
            log_bytes / 500e6,
            log_bytes / 1e9
        );
    }
}

/// Median of the repeats, which is the figure an extrapolation uses — `Samples` prints the spread beside it,
/// and the spread is what says whether the median means anything.
fn median(mut elapsed: Vec<Duration>) -> Duration {
    elapsed.sort();
    elapsed[elapsed.len() / 2]
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    dump_rate(&options);
    recovery_rate(&options);
}
