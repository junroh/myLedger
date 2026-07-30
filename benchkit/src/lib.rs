//! Shared harness for the crates' own benchmarks: repeat, report median, and say which
//! thread placement the numbers were taken under.

use std::time::Duration;

use ledger_base::ThreadPolicy;

/// Odd multiplier: consecutive steps land on unrelated cache lines, which is what real
/// account traffic does and what prefetching cannot follow.
pub const STRIDE: u64 = 2_654_435_761;

pub struct Samples {
    name: String,
    ops: u64,
    elapsed: Vec<Duration>,
}

impl Samples {
    pub fn new(name: String, ops: u64) -> Self {
        Self { name, ops, elapsed: Vec::new() }
    }

    pub fn add(&mut self, elapsed: Duration) {
        self.elapsed.push(elapsed);
    }

    pub fn report(&mut self) {
        self.elapsed.sort();
        let nanos_per_op = |elapsed: Duration| elapsed.as_nanos() as f64 / self.ops as f64;
        let median = nanos_per_op(self.elapsed[self.elapsed.len() / 2]);
        let fastest = nanos_per_op(self.elapsed[0]);
        let slowest = nanos_per_op(self.elapsed[self.elapsed.len() - 1]);
        // The spread is what says whether a difference between two lines means anything: a machine
        // that varies 20% between repeats cannot show a 10% change.
        let spread = (slowest - fastest) / median * 100.0;
        println!(
            "{:<38} {:>7.1} {:>7.1} {:>7.1} {:>6.0}% {:>9.2} M ops/s  (n={})",
            self.name,
            median,
            fastest,
            slowest,
            spread,
            1_000.0 / median,
            self.elapsed.len()
        );
    }
}

pub struct BenchOptions {
    pub repeat: usize,
    pub pin: Option<usize>,
}

impl BenchOptions {
    pub fn from_args() -> Self {
        let mut options = Self { repeat: 3, pin: None };
        let args: Vec<String> = std::env::args().skip(1).collect();
        for index in 0..args.len() {
            let value = args.get(index + 1).and_then(|text| text.parse().ok());
            match args[index].as_str() {
                "--repeat" => options.repeat = value.unwrap_or(options.repeat).max(1),
                "--pin" => options.pin = value,
                _ => {}
            }
        }
        options
    }

    /// Applies the placement and prints the header both bench binaries share.
    pub fn announce(&self) {
        println!("thread placement: {}", ThreadPolicy::apply(self.pin));
        println!(
            "{:<38} {:>7} {:>7} {:>7} {:>7} {:>9}",
            "benchmark", "median", "min", "max", "spread", "median"
        );
    }
}
