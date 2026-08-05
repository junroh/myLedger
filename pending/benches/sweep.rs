//! What the expiry sweep costs to walk the index, which is the cost the design has no number for.
//!
//! A day is finished by a pass over the index that finds nothing pointing into it, and `expiring` bounds
//! that pass by the voids it collects, not by the slots it visits. So a segment with few survivors left —
//! which every day becomes on its way to empty — costs a walk most of the table wide, on the worker's own
//! thread, ahead of the commands it would otherwise be draining. This bench is the per-slot cost of that
//! walk and the slots one day's expiry needs, so the stall can be stated in milliseconds instead of
//! inferred.
//!
//! Warm tables only: the walk is also the first thing to touch a freshly allocated table, and a run that
//! measured the first pass would be reporting page faults. Each table here is walked once before it is
//! timed.

use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_base::TxId;
use ledger_benchkit::{BenchOptions, Samples, STRIDE};
use ledger_pending::{BlockAddr, HoldTable, RECORDS_PER_BLOCK, SEGMENTS, SLOT_BYTES};

/// Slots the design's index has: 4.8 billion holds at the 0.90 load target. Nothing here allocates it —
/// 37GB — so it is the number the measured per-slot cost is multiplied by, and the flatness across the
/// sizes that *are* allocated is what says that multiplication is allowed.
const DESIGN_SLOTS: u64 = 5_333_333_333;

/// Voids one round collects. The engine's default (`expiry_per_round`), so the rounds counted here are the
/// rounds it would run.
const PER_ROUND: usize = 64;

/// Days of the design's retention, as segments hold them: one segment per live day. The fill spreads over
/// this many, which is what makes the sweep's target one day's share of the table rather than all of it.
const LIVE_SEGMENTS: u64 = 33;

const _: () = assert!(
    LIVE_SEGMENTS < SEGMENTS,
    "the fill needs live days the address format has segments for"
);

/// The load factor the index is sized against, and the only one an extrapolation may use.
const TARGET: f64 = ledger_pending::LOAD_TARGET;

/// The table every row but the size sweep is measured on. Large enough to be well past the last level of
/// cache, small enough to fill in seconds.
const SLOTS: usize = 1 << 26;

/// The segment the sweep is emptying. Day zero, so the fill can be asked for a chosen number of holds in
/// it and put the rest elsewhere.
const EXPIRING: u8 = 0;

/// Transaction ids are not dense, and the walk has to behave the same for keys that are far apart as for
/// keys that are not — the odd multiplier is a bijection, so the keys stay distinct.
fn key(ordinal: u64) -> TxId {
    TxId(u128::from(ordinal.wrapping_mul(STRIDE)) + 1)
}

/// The ordinal a hold was created with, carried in the address bits the index keeps. The sweep answers
/// with addresses and the engine reads the record they name to recover the key; a bench that kept a map
/// instead would need an entry per hold at the sizes measured here, which is the memory the index exists
/// to avoid.
fn address(segment: u8, ordinal: u64) -> BlockAddr {
    let per_block = RECORDS_PER_BLOCK as u64;
    BlockAddr::new(segment, ordinal / per_block, (ordinal % per_block) as u8)
}

fn ordinal(addr: BlockAddr) -> u64 {
    addr.block() * RECORDS_PER_BLOCK as u64 + u64::from(addr.index())
}

/// A table at a load factor, with `in_day` of its entries in the expiring segment and the rest spread over
/// the other live days. Scattered across the whole table in both cases, because the bucket a key lands in
/// has nothing to do with the day it arrived — which is why a day's survivors cannot be walked without
/// walking everything else.
fn filled(slots: usize, load_factor: f64, in_day: u64) -> HoldTable {
    let mut table = HoldTable::with_slots(slots);
    let holds = (table.slots() as f64 * load_factor) as u64;
    for at in 0..holds {
        let segment = if at < in_day {
            EXPIRING
        } else {
            (EXPIRING as u64 + 1 + at % (LIVE_SEGMENTS - 1)) as u8
        };
        let _ = table.insert_new(key(at), address(segment, at));
    }
    table
}

/// A day's share of a full table, which is what the segment holds when its day starts expiring.
fn day_share(table: &HoldTable) -> u64 {
    table.live() as u64 / LIVE_SEGMENTS
}

/// One pass of the index for a segment that holds nothing: every slot visited, nothing collected. This is
/// the pass that ends a day, and the cost in the sweep that no bound in the code applies to.
fn empty_pass(table: &HoldTable, found: &mut Vec<BlockAddr>) -> Duration {
    found.clear();
    let started = Instant::now();
    let at = table.addresses_in_segment(black_box(EXPIRING), 0, PER_ROUND, found);
    let elapsed = started.elapsed();
    assert_eq!(at, table.slots(), "a pass that found nothing stopped early");
    assert!(found.is_empty(), "the segment was supposed to be empty");
    elapsed
}

/// Median of the repeats, which is the figure an extrapolation uses — `Samples` prints the spread beside
/// it, and the spread is what says whether the median means anything.
fn median(mut elapsed: Vec<Duration>) -> Duration {
    elapsed.sort();
    elapsed[elapsed.len() / 2]
}

fn timed_pass(
    options: &BenchOptions,
    table: &HoldTable,
    name: String,
    found: &mut Vec<BlockAddr>,
) -> f64 {
    empty_pass(table, found);
    let mut samples = Samples::new(name, table.slots() as u64);
    let mut elapsed = Vec::new();
    for _ in 0..options.repeat {
        let one = empty_pass(table, found);
        elapsed.push(one);
        samples.add(one);
    }
    samples.report();
    median(elapsed).as_nanos() as f64 / table.slots() as f64
}

/// Per-slot cost against table size. Flat once the table is past the last level of cache is what allows
/// the design's 37GB table to be spoken about from a 2GB one: the walk is sequential and reads each slot
/// once, so if the cost per slot stops changing, size only multiplies it. A row that is not flat is the
/// row that refuses the extrapolation.
fn scan_rate(options: &BenchOptions, found: &mut Vec<BlockAddr>) -> f64 {
    println!();
    println!("one pass of the index, expiring segment empty — the pass that ends a day");
    let mut nanos_per_slot = 0.0;
    for exponent in [22u32, 24, 26, 28] {
        let table = filled(1usize << exponent, TARGET, 0);
        nanos_per_slot = timed_pass(
            options,
            &table,
            format!(
                "scan  {:>10} slots ({:>6.1}MB)",
                table.slots(),
                (table.slots() * SLOT_BYTES) as f64 / 1e6
            ),
            found,
        );
        println!(
            "    {:.1}ms here, {:.2}s at the design's {DESIGN_SLOTS} slots",
            nanos_per_slot * table.slots() as f64 / 1e6,
            nanos_per_slot * DESIGN_SLOTS as f64 / 1e9
        );
    }
    nanos_per_slot
}

/// The same pass against how full the table is. An empty slot is skipped before its address is unpacked,
/// so an empty table is the cheapest walk and the load target is the honest one — this row is here so the
/// number the extrapolation uses is known to be the expensive end rather than assumed to be.
fn scan_against_fill(options: &BenchOptions, found: &mut Vec<BlockAddr>) {
    println!();
    println!("the same pass, against how full the table is");
    for load_factor in [0.0, 0.45, TARGET] {
        let table = filled(SLOTS, load_factor, 0);
        let name = format!(
            "scan  lf {:.2} ({} live)",
            table.load_factor(),
            table.live()
        );
        timed_pass(options, &table, name, found);
    }
}

/// What emptying one day costs: rounds, slots walked, and the worst single round that found something.
///
/// Optimistic in one way that matters: a void here releases its slot in the round that found it, where the
/// engine's is judged, committed and applied several queues later — and `Sweep` re-walks from the start
/// each pass, so the engine sees the thinning segment this hides. That makes the total a floor, and a
/// floor that does not fit the budget is enough to answer the question.
struct Day {
    rounds: u64,
    slots_walked: u64,
    voids: u64,
    /// The most slots one round walked while still finding voids. Kept apart from the pass that ends the
    /// day, because that pass always walks everything and is already the row above — what this says is
    /// whether a *productive* round is bounded, which is what the throttle believes it is paying for.
    worst_productive: u64,
}

fn empty_one_day(table: &mut HoldTable, found: &mut Vec<BlockAddr>) -> Day {
    let slots = table.slots();
    let mut day = Day {
        rounds: 0,
        slots_walked: 0,
        voids: 0,
        worst_productive: 0,
    };
    let mut at = 0;
    let mut anything_this_pass = false;
    loop {
        found.clear();
        let reached = table.addresses_in_segment(EXPIRING, at, PER_ROUND, found);
        let walked = (reached - at) as u64;
        day.rounds += 1;
        day.slots_walked += walked;
        day.voids += found.len() as u64;
        if !found.is_empty() {
            day.worst_productive = day.worst_productive.max(walked);
            anything_this_pass = true;
        }
        for &addr in found.iter() {
            table.remove(key(ordinal(addr)), &mut |candidate| candidate == addr);
        }
        at = reached;
        if at < slots {
            continue;
        }
        if !anything_this_pass {
            return day;
        }
        at = 0;
        anything_this_pass = false;
    }
}

/// A day of expiry at three densities: the day's whole share, a day nearly finished, and a day with fewer
/// left than one round collects. The last is not an edge case — every day passes through it on the way to
/// empty, and it is where a round stops being bounded by `expiry_per_round` at all.
fn day_of_expiry(nanos_per_slot: f64, found: &mut Vec<BlockAddr>) {
    println!();
    println!(
        "{:<40} {:>8} {:>13} {:>15} {:>10}",
        format!("emptying one day, {PER_ROUND} voids per round"),
        "rounds",
        "slots walked",
        "worst product.",
        "that round"
    );
    let full = filled(SLOTS, TARGET, 0);
    for in_day in [day_share(&full), 10_000, (PER_ROUND / 2) as u64] {
        let mut table = filled(SLOTS, TARGET, in_day);
        let day = empty_one_day(&mut table, found);
        let stall = day.worst_productive as f64 * nanos_per_slot;
        println!(
            "{:<40} {:>8} {:>13} {:>15} {:>9.1}ms",
            format!("{} in the day, {} voids offered", in_day, day.voids),
            day.rounds,
            day.slots_walked,
            day.worst_productive,
            stall / 1e6
        );
        println!(
            "    at the design's {DESIGN_SLOTS} slots that round is {:.2}s, the whole day {:.1}s",
            day.worst_productive as f64 / table.slots() as f64
                * DESIGN_SLOTS as f64
                * nanos_per_slot
                / 1e9,
            day.slots_walked as f64 / table.slots() as f64 * DESIGN_SLOTS as f64 * nanos_per_slot
                / 1e9
        );
    }
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    // One buffer for every round of every row: the engine reuses one too, so a sweep allocates nothing
    // however often it is asked.
    let mut found = Vec::with_capacity(PER_ROUND);

    let nanos_per_slot = scan_rate(&options, &mut found);
    scan_against_fill(&options, &mut found);
    day_of_expiry(nanos_per_slot, &mut found);
}
