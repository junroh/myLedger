use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Time is injected so batching decisions can be replayed exactly in a simulation.
pub trait Clock {
    fn now_nanos(&self) -> u64;
}

#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_nanos(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }
}

/// Advanced by hand, for tests and deterministic simulation. Clones share the same time, so
/// the reactor can hold one while the test moves it.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    nanos: Arc<AtomicU64>,
}

impl ManualClock {
    pub fn new(nanos: u64) -> Self {
        Self { nanos: Arc::new(AtomicU64::new(nanos)) }
    }

    pub fn advance(&self, nanos: u64) {
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
    }
}

impl Clock for ManualClock {
    fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }
}
