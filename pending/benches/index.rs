use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_base::{FxHashMap, TxId};
use ledger_benchkit::{BenchOptions, Samples, STRIDE};
use ledger_pending::{BlockAddr, HoldTable};

const OPS: u64 = 5_000_000;

/// The cap the table uses by default, for a line that has to say what the worst chain is measured
/// against.
const MAX_HOPS_REPORTED: u32 = 128;

/// Transaction ids are not dense, and the index has to behave the same for keys that are far apart
/// as for keys that are not — the odd multiplier is a bijection, so the keys stay distinct.
fn key(index: usize) -> TxId {
    TxId(u128::from((index as u64).wrapping_mul(STRIDE)) + 1)
}

/// The table's size is chosen first and the fill follows it. Asking for a load factor and rounding
/// the table up to the next power of two quantises the answer instead: at a million holds every
/// target from a half to one lands on the same table, so four rows would report one load factor.
fn filled(slots: usize, load_factor: f64) -> (HoldTable, usize) {
    let holds = (slots as f64 * load_factor) as usize;
    let mut table = HoldTable::with_slots(slots);
    for index in 0..holds {
        let _ = table.insert_new(key(index), BlockAddr::from_raw(index as u64));
    }
    (table, holds)
}

/// A lookup of a hold that is there: two buckets' worth of fingerprints compared, then one record
/// read to confirm the key. This is the cost on the path a resolution cannot avoid.
fn lookup_hit(table: &HoldTable, holds: usize) -> Duration {
    let started = Instant::now();
    let mut found = 0u64;
    for step in 0..OPS {
        let index = step.wrapping_mul(STRIDE) as usize % holds;
        if let Some(addr) = table.addr_of(black_box(key(index)), &mut |_| true) {
            found += addr.raw();
        }
    }
    black_box(found);
    started.elapsed()
}

/// A lookup of a hold that is not there, which is the worse case: both candidate buckets are read in
/// full before the answer is no.
fn lookup_miss(table: &HoldTable) -> Duration {
    let started = Instant::now();
    let mut absent = 0u64;
    for step in 0..OPS {
        let key = TxId(u128::MAX - u128::from(step.wrapping_mul(STRIDE)));
        absent += u64::from(table.addr_of(black_box(key), &mut |_| true).is_none());
    }
    black_box(absent);
    started.elapsed()
}

fn map_lookup_hit(map: &FxHashMap<TxId, BlockAddr>, holds: usize) -> Duration {
    let started = Instant::now();
    let mut found = 0u64;
    for step in 0..OPS {
        let index = step.wrapping_mul(STRIDE) as usize % holds;
        if let Some(addr) = map.get(&black_box(key(index))) {
            found += addr.raw();
        }
    }
    black_box(found);
    started.elapsed()
}

/// How many entries a table filled to a target load factor cannot place at all — the number a stash has
/// to hold, measured rather than assumed. Growing is disabled here: with it, the first failure doubles
/// the table and the pressure vanishes, which is why the rate above says nothing about a stash.
fn stash_demand(options: &BenchOptions) {
    println!();
    println!(
        "{:<28} {:>12} {:>10} {:>12} {:>12}",
        "unplaceable", "holds", "homeless", "per million", "worst chain"
    );
    for max_hops in [32u32, 64, 128, 256] {
        for load_factor in [0.90, 0.93, 0.95] {
            let slots = 1usize << 22;
            let mut homeless = 0u64;
            let mut holds_total = 0u64;
            let mut worst = 0u32;
            for repeat in 0..options.repeat {
                let holds = (slots as f64 * load_factor) as usize;
                let mut table = HoldTable::with_capacity(slots, max_hops);
                let salt = (repeat as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                for index in 0..holds {
                    let key = TxId(u128::from(salt ^ (index as u64).wrapping_mul(STRIDE)) + 1);
                    if table.insert_new(key, BlockAddr::from_raw(index as u64)).is_err() {
                        homeless += 1;
                    }
                }
                let (_, chain) = table.kick_stats();
                worst = worst.max(chain);
                holds_total += holds as u64;
            }
            println!(
                "{:<28} {:>12} {:>10} {:>12.1} {:>12}",
                format!("cap={max_hops} lf={load_factor:.2}"),
                holds_total,
                homeless,
                homeless as f64 / holds_total as f64 * 1e6,
                worst
            );
        }
    }
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    stash_demand(&options);

    for slots in [1usize << 17, 1 << 20, 1 << 23] {
        for load_factor in [0.5, 0.90] {
            let (table, holds) = filled(slots, load_factor);
            let label = format!("lf={:.2} ({holds} holds)", table.load_factor());
            let mut samples = Samples::new(format!("lookup hit  {label}"), OPS);
            for _ in 0..options.repeat {
                samples.add(lookup_hit(&table, holds));
            }
            samples.report();

            let mut samples = Samples::new(format!("lookup miss {label}"), OPS);
            for _ in 0..options.repeat {
                samples.add(lookup_miss(&table));
            }
            samples.report();
        }
    }

    // The claim a bucketed cuckoo index is making is not that it is faster on average. Both structures
    // answer the same question — where is this hold — so the comparison is honest; what the index buys
    // is a bound on the probe and an eight-byte slot, where a map pays a key and an address per entry
    // plus its own spare capacity. The comparison is here so the price of that is re-runnable rather
    // than remembered.
    for slots in [1usize << 17, 1 << 20] {
        let holds = (slots as f64 * 0.90) as usize;
        let mut map: FxHashMap<TxId, BlockAddr> = FxHashMap::default();
        for index in 0..holds {
            map.insert(key(index), BlockAddr::from_raw(index as u64));
        }
        let mut samples = Samples::new(format!("lookup hit  FxHashMap ({holds} holds)"), OPS);
        for _ in 0..options.repeat {
            samples.add(map_lookup_hit(&map, holds));
        }
        samples.report();
    }

    // Insertion is the apply path, off the resolution's critical path, which is why a kick chain is
    // the affordable half of the trade. How long it gets as the table fills is what an analytic
    // estimate of it was guessing.
    const SLOTS: usize = 1 << 20;
    for load_factor in [0.5, 0.75, 0.90, 0.95] {
        let holds = (SLOTS as f64 * load_factor) as usize;
        let mut samples = Samples::new(
            format!("insert      lf={load_factor:.2} ({holds} holds)"),
            holds as u64,
        );
        let mut last = None;
        for _ in 0..options.repeat {
            let started = Instant::now();
            let (table, _) = filled(SLOTS, load_factor);
            samples.add(started.elapsed());
            last = Some(table);
        }
        samples.report();
        if let Some(table) = last {
            let (hops, worst) = table.kick_stats();
            println!(
                "    kicks: {:.3} hops per insert, worst {worst} of {} cap, load factor {:.3}",
                hops as f64 / holds as f64,
                MAX_HOPS_REPORTED,
                table.load_factor()
            );
        }
    }
}
