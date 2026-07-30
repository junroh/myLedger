use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Stub workers poll their queues; this keeps an idle worker off the CPU without adding
/// latency while work is flowing.
pub struct IdleBackoff {
    idle_rounds: u32,
}

impl IdleBackoff {
    const SPIN_ROUNDS: u32 = 64;
    const SLEEP: Duration = Duration::from_micros(20);

    pub const fn new() -> Self {
        Self { idle_rounds: 0 }
    }

    pub fn record(&mut self, made_progress: bool) {
        if made_progress {
            self.idle_rounds = 0;
            return;
        }
        self.idle_rounds = self.idle_rounds.saturating_add(1);
        if self.idle_rounds <= Self::SPIN_ROUNDS {
            thread::yield_now();
        } else {
            thread::sleep(Self::SLEEP);
        }
    }
}

impl Default for IdleBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns one external path's execution context. Each path gets its own thread so a slow
/// path cannot delay a fast one.
pub struct WorkerThread {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl WorkerThread {
    pub fn spawn<F>(name: &str, body: F) -> Self
    where
        F: FnOnce(Arc<AtomicBool>) + Send + 'static,
    {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let handle = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || body(flag))
            .ok();
        Self { shutdown, handle }
    }
}

impl Drop for WorkerThread {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
