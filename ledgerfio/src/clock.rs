use std::time::Instant;

/// The client's own clock. The ledger only carries the submit stamp back, so latency is
/// measured entirely on this side.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    origin: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub fn nanos(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}
