use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;

/// Catches interrupt and terminate for whoever owns the process.
///
/// Installing this is a binary's decision, never a library's: which signal means "stop" belongs to
/// the process owner, and the ledger service only exposes a stop token to be triggered.
pub struct Signals;

static REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

impl Signals {
    /// Registered through `sigaction`, so the handler survives the first signal and the syscalls it
    /// interrupts are restarted. Calling it twice is harmless.
    pub fn install() {
        let flag = REQUESTED.get_or_init(|| Arc::new(AtomicBool::new(false)));
        for signal in [SIGINT, SIGTERM] {
            let _ = flag::register(signal, Arc::clone(flag));
        }
    }

    pub fn requested() -> bool {
        REQUESTED.get().is_some_and(|flag| flag.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stop request has to survive being asked for: the flag is set from the handler and stays
    /// set, which is what lets a run finish its drain instead of dying mid-batch.
    #[test]
    fn a_terminate_signal_asks_the_process_to_stop() {
        assert!(!Signals::requested(), "nothing asked yet");
        Signals::install();

        signal_hook::low_level::raise(SIGTERM).expect("raise");
        // The handler runs on this thread before `raise` returns.
        assert!(Signals::requested());
        assert!(Signals::requested(), "the request is not consumed by reading it");
    }
}
