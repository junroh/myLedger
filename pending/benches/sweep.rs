//! What the expiry sweep costs, which the design has no number for.
//!
//! A day is finished when nothing in the index points into it, and the survivors that have to be released
//! first are found by reading that day's own blocks. So the sweep's cost is the day's records, not the
//! index's slots — and this measures it on the real engine, because the two costs it adds up are the block
//! read and the index probe that decides whether each record is still alive.
//!
//! **What this replaced, and why the number mattered.** Searching the index for addresses in the expiring
//! segment bounded the voids a round *collected* and not the slots it *visited*, so a day thinning towards
//! empty walked most of the table per round and the pass that ended a day walked all of it: 0.42ns a slot,
//! flat from 33MB to 2.1GB, which is 2.2 seconds at the design's 5.33 billion slots — against a 5ms speed
//! contract, on the thread that answers lookups. That measurement is gone with the method it measured; what
//! is here is the replacement's cost, in the same units, so the two can be compared.

use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_base::ports::{ApplyIndex, PendingEffect};
use ledger_base::{AccountId, BudgetGroup, Transfer, TxId};
use ledger_benchkit::{BenchOptions, Samples, STRIDE};
use ledger_pending::{MemoryStore, PendingEngine, RECORDS_PER_BLOCK};

/// Holds alive at once in the design: 4.8 billion over its retention. The per-record cost measured here is
/// multiplied by one day's share of it, which is what a day's sweep has to get through.
const DESIGN_HOLDS: u64 = 4_800_000_000;

/// Days of the design's retention, so a day's share of the table is the survivors one sweep faces.
const LIVE_DAYS: u64 = 33;

/// Retention and grace, as the engine is told them. Two days plus one is the smallest lifetime that has a
/// day to expire and a day to still be writing, which is all a bench needs — the walk does not care how
/// many days there are, only how much one of them wrote.
const RETENTION_DAYS: u64 = 2;
const LIFETIME_DAYS: u64 = RETENTION_DAYS + 1;

/// Transaction ids are not dense, and the walk has to behave the same for keys that are far apart as for
/// keys that are not — the odd multiplier is a bijection, so the keys stay distinct.
fn key(ordinal: u64) -> TxId {
    TxId(u128::from(ordinal.wrapping_mul(STRIDE)) + 1)
}

fn create(tx_id: TxId) -> PendingEffect {
    PendingEffect::Create {
        tx_id,
        debit_account: AccountId(1),
        credit_account: AccountId(2),
        amount: 100,
        ledger: 1,
        budget: BudgetGroup::ABSENT,
    }
}

fn remove(pending_ref: TxId) -> PendingEffect {
    PendingEffect::Remove {
        pending_ref,
        released: 100,
        budget: BudgetGroup::ABSENT,
    }
}

/// An engine holding `holds` holds, all of them flushed into day zero's blocks and all of them still alive,
/// with the calendar moved on far enough that day zero has run out.
///
/// The flush window is one block, so every record leaves the buffer and reaches a segment: a window that
/// held them would leave the index addressing the buffer instead, and a sweep of a day would find nothing —
/// which is a real property of the engine and would be a silent zero here.
fn expiring_day(holds: u64) -> PendingEngine {
    let slots = (holds as f64 / ledger_pending::LOAD_TARGET) as usize;
    let mut engine = PendingEngine::sized(slots, 1, 1, Box::new(MemoryStore::default()));
    engine.open_day(0, LIFETIME_DAYS);
    for at in 0..holds {
        engine
            .write(create(key(at)), ApplyIndex(at + 1))
            .expect("the index took the hold");
    }
    // The day the buffer is drained into is still day zero, so a roll to the next day seals the last block
    // into it before anything else is written.
    for day in 1..=LIFETIME_DAYS {
        engine.open_day(day, LIFETIME_DAYS);
    }
    engine
}

/// Emptying one day: rounds until the day's blocks are all read, then the voids applied, then the round
/// that finds the day empty and hands its blocks back.
///
/// The applies are the part a bench has to do itself, and they are what makes this the whole cost rather
/// than the walk alone: in the ledger a void is judged and committed before the engine is told, and until it
/// is told the hold is still in the index and the day is not done.
struct Day {
    rounds: u64,
    blocks_read: u64,
    voids: u64,
    freed: bool,
}

/// The per-record cost of the walk against how many holds the day has. Flat is what says the cost is the
/// day's own records rather than anything that grows with the table — the property the index scan did not
/// have.
fn walk_cost(options: &BenchOptions) {
    println!();
    println!("emptying one day of holds, two blocks a round");
    let mut voids = Vec::new();
    for holds in [100_000u64, 400_000, 1_600_000] {
        let mut samples = Samples::new(format!("sweep {holds:>9} holds in the day"), holds);
        let mut last = None;
        for _ in 0..options.repeat {
            let mut engine = expiring_day(holds);
            let started = Instant::now();
            let (day, _) = empty_one_day_timed(&mut engine, 2, &mut voids);
            let elapsed = started.elapsed();
            samples.add(elapsed);
            assert!(day.freed, "the day never emptied");
            // Everything but the block still being filled: the writeback buffer always holds its newest
            // block, and a record in there addresses the buffer rather than a day — so it belongs to
            // whichever day it is eventually flushed into, not to this one. The rest have to be found.
            assert!(
                day.voids >= holds - RECORDS_PER_BLOCK as u64,
                "the walk missed live holds: {} of {holds}",
                day.voids
            );
            last = Some((day, elapsed));
        }
        samples.report();
        let Some((day, elapsed)) = last else { continue };
        let per_hold = elapsed.as_nanos() as f64 / day.voids as f64;
        let design_day = DESIGN_HOLDS / LIVE_DAYS;
        println!(
            "    {} rounds, {} blocks read for {} voids ({:.1} records read per void), \
             {:.1}s for a design day's {design_day}",
            day.rounds,
            day.blocks_read,
            day.voids,
            day.blocks_read as f64 * RECORDS_PER_BLOCK as f64 / day.voids as f64,
            per_hold * design_day as f64 / 1e9,
        );
    }
}

/// Records read per void released, against how much of the day is still alive when it expires. This is the
/// ratio a throttle policy has to be sized from: a day's blocks hold what survived the flush window, and by
/// the time the day runs out some of those have been resolved the ordinary way — their records are still on
/// the blocks and still have to be read past.
fn survivor_density(options: &BenchOptions) {
    println!();
    println!("records read per void, against how much of the day is still alive");
    const HOLDS: u64 = 400_000;
    let mut voids = Vec::new();
    // One survivor in every `spacing`, spread through the day rather than gathered at one end. Records sit
    // on blocks in the order they were appended, so resolving the *last* nine tenths would leave every
    // survivor in the first tenth of the blocks and the walk would stop there — a number that flatters the
    // walk by assuming what a real day never does.
    for spacing in [1u64, 2, 10, 100] {
        let mut worst = Duration::ZERO;
        let mut day = None;
        for _ in 0..options.repeat {
            let mut engine = expiring_day(HOLDS);
            // Resolved before the day ran out, which is what most holds do. The record stays where it is —
            // blocks are written once — so the walk still reads it and the index still says it is dead.
            for at in 0..HOLDS {
                if !at.is_multiple_of(spacing) {
                    let _ = engine.write(remove(key(at)), ApplyIndex(at + 1));
                }
            }
            let (finished, round) = empty_one_day_timed(&mut engine, 2, &mut voids);
            worst = worst.max(round);
            day = Some(finished);
        }
        let Some(day) = day else { continue };
        println!(
            "  {:>5.1}% alive: {:>7} voids from {:>6} blocks read, {:>6.1} records per void, worst \
             walking round {:>6.1}us",
            100.0 / spacing as f64,
            day.voids,
            day.blocks_read,
            day.blocks_read as f64 * RECORDS_PER_BLOCK as f64 / day.voids.max(1) as f64,
            worst.as_nanos() as f64 / 1e3
        );
    }
}

/// The same day at several round sizes. What one round costs is what has to fit beside the lookups the
/// engine is answering, and it is bounded by declaration now — so this row is the bound being linear, which
/// is the property the index scan did not have at any setting.
///
/// A round is both halves of the worker's own sweep and nothing is kept out of the timer. It used to be:
/// handing a day's blocks back meant a scan of the store's map, a stand-in's cost that would have been the
/// worst round at every size and hidden the one being measured. A segment now stops existing in one call.
fn round_size(options: &BenchOptions) {
    println!();
    println!("the same day, against the blocks a round reads");
    const HOLDS: u64 = 400_000;
    let mut voids = Vec::new();
    for blocks in [1usize, 2, 8, 64] {
        let mut worst = Duration::ZERO;
        let mut rounds = 0;
        for _ in 0..options.repeat {
            let mut engine = expiring_day(HOLDS);
            let (day, round) = empty_one_day_timed(&mut engine, blocks, &mut voids);
            worst = worst.max(round);
            rounds = day.rounds;
        }
        println!(
            "  {blocks:>3} blocks a round: worst walking round {:>7.1}us, {rounds:>7} rounds, at most \
             {} voids offered",
            worst.as_nanos() as f64 / 1e3,
            blocks * RECORDS_PER_BLOCK
        );
    }
}

/// Emptying a day and the worst round that read blocks. Rounds that offered nothing are left out of the
/// worst: they are the wait for the day to empty, which is not the walk.
fn empty_one_day_timed(
    engine: &mut PendingEngine,
    blocks: usize,
    voids: &mut Vec<Transfer>,
) -> (Day, Duration) {
    let before = engine.swept_blocks();
    let mut worst = Duration::ZERO;
    let mut day = Day {
        rounds: 0,
        blocks_read: 0,
        voids: 0,
        freed: false,
    };
    while day.rounds < 1_000_000 {
        voids.clear();
        // Both halves, the way the worker runs them, and both inside the timer: reclaiming a dead day's
        // blocks is every node's own housekeeping and proposing its voids is the leader's, so a round that
        // timed only the second would be timing a node that never gets its space back. Reclaim used to sit
        // outside it because handing a day back meant a scan of the store's map; a segment now stops
        // existing in one call, so there is nothing left to keep out of the measurement.
        let started = Instant::now();
        engine.reclaim();
        engine.propose_expiry(black_box(blocks), voids);
        let elapsed = started.elapsed();
        day.rounds += 1;
        day.voids += voids.len() as u64;
        if !voids.is_empty() {
            worst = worst.max(elapsed);
        }
        for void in voids.iter() {
            engine
                .write(remove(void.pending_ref), ApplyIndex(day.rounds + 1))
                .expect("a void removes rather than inserts");
        }
        if !engine.sweeping() {
            day.freed = true;
            break;
        }
    }
    day.blocks_read = engine.swept_blocks() - before;
    (day, worst)
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    walk_cost(&options);
    survivor_density(&options);
    round_size(&options);
}
