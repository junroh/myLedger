use std::time::{Duration, Instant};

use ledger_base::Prng;

#[derive(Debug, Clone, Copy)]
pub struct LatencyRange {
    pub min: Duration,
    pub max: Duration,
}

impl LatencyRange {
    pub const fn new(min: Duration, max: Duration) -> Self {
        Self { min, max }
    }

    pub const fn fixed(value: Duration) -> Self {
        Self {
            min: value,
            max: value,
        }
    }

    pub fn sample(&self, prng: &mut Prng) -> Duration {
        let span = self.max.as_nanos().saturating_sub(self.min.as_nanos()) as u64;
        if span == 0 {
            return self.min;
        }
        self.min + Duration::from_nanos(prng.next_u64() % (span + 1))
    }

    pub fn due_from(&self, now: Instant, prng: &mut Prng) -> Instant {
        now + self.sample(prng)
    }
}
