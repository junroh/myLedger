use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_base::Prng;
use ledger_benchkit::{BenchOptions, Samples};

const STEPS: u64 = 20_000_000;

/// What one dependent memory access costs at each working-set size. This is the machine constant the
/// simulator's cost model needs: the per-stage cost of the ledger is compute plus some number of
/// misses, and a miss costs whatever this bench says it does on the hardware in hand.
///
/// A pointer chase, because it is the only shape a prefetcher cannot follow: each access decides the
/// address of the next one, so the measured time is latency and not bandwidth. The chase visits every
/// slot exactly once before repeating, so the working set is exactly the size claimed.
struct Chase {
    bytes: usize,
}

impl Chase {
    /// One cycle through every slot, in an order drawn from the seed. `next[i]` holds the index to
    /// visit after `i`, so following it is a single dependent load per step.
    fn build(slots: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..slots).collect();
        let mut prng = Prng::new(0x00DD_BA11);
        // Fisher-Yates, so every slot appears once: a chase that revisits early would measure a
        // smaller working set than the one it claims.
        for index in (1..slots).rev() {
            let other = (prng.next_u64() % (index as u64 + 1)) as usize;
            order.swap(index, other);
        }
        let mut next = vec![0usize; slots];
        for pair in order.windows(2) {
            next[pair[0]] = pair[1];
        }
        next[order[slots - 1]] = order[0];
        next
    }

    fn run(&self) -> Duration {
        let slots = self.bytes / size_of::<usize>();
        let next = Self::build(slots);
        // Warm the translation and the caches that will hold it, so the first pass is not measured.
        let mut cursor = 0;
        for _ in 0..slots {
            cursor = next[cursor];
        }
        let started = Instant::now();
        for _ in 0..STEPS {
            cursor = next[cursor];
        }
        let elapsed = started.elapsed();
        black_box(cursor);
        elapsed
    }
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    println!("one dependent access, by working set — the last line is what a miss costs");
    for kib in [16usize, 256, 8 << 10, 256 << 10] {
        let name = if kib >= 1 << 10 {
            format!("chase {} MiB", kib >> 10)
        } else {
            format!("chase {kib} KiB")
        };
        let mut samples = Samples::new(name, STEPS);
        for _ in 0..options.repeat {
            samples.add(Chase { bytes: kib << 10 }.run());
        }
        samples.report();
    }
}
