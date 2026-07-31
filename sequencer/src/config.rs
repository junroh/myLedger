use std::time::Duration;

use ledger_base::LedgerError;

/// Sizing, not policy: caps memory and decides when backpressure starts.
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    /// Requests in flight at once: the memory bound, and where intake starts refusing.
    pub slots: usize,
    /// Caps how long intake holds the loop. Not a throughput limit.
    pub intake_per_tick: usize,
    /// Acks buffered when the client is not draining. Reaching it pauses intake.
    pub ack_backlog: usize,
    /// Hold decisions buffered when the pending engine is not draining. Also pauses intake.
    pub pending_write_backlog: usize,
    /// Log events buffered. Overflow drops and counts them; it never slows the reactor.
    pub log_events: usize,
}

/// Large batches hide the consensus round trip; a cap keeps them from trading it for latency.
#[derive(Debug, Clone, Copy)]
pub struct BatchPolicy {
    /// Effects that make a batch full.
    pub size: usize,
    /// Ceiling on one proposal. When judging outruns consensus, the batch is cut here, at a chain
    /// boundary.
    pub max: usize,
    /// Judged effects allowed to wait for consensus before intake pauses. `max` bounds what one
    /// proposal carries, which is a different thing: while `in_flight` proposals are already
    /// outstanding no further one can be taken at all, so without this the buffer of judged effects
    /// grows for as long as consensus is slow. Reaching it is backpressure — intake stops, the client
    /// feels it, and what is here gets proposed — not a refusal.
    pub queued: usize,
    /// How long a partial batch waits. The latency floor at low load.
    pub linger: Duration,
    /// Proposals in flight at once; times `size`, the work hidden behind one round trip.
    pub in_flight: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct LinkedPolicy {
    /// Legs one chain may have. Bounds the judge's work and the batch overshoot.
    pub max_legs: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct SafetyPolicy {
    /// Quarantined lanes that mean the component is broken, not one lane. Reaching it fail-stops.
    pub quarantine_fail_stop: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ReactorConfig {
    pub capacity: Capacity,
    pub batching: BatchPolicy,
    pub linked: LinkedPolicy,
    pub safety: SafetyPolicy,
    /// Time each stage of the tick. Answers "which stage is the bottleneck" at the cost of a
    /// clock read per stage, so it is a measurement mode, not a production setting.
    pub profile: bool,
}

impl Default for ReactorConfig {
    fn default() -> Self {
        Self {
            capacity: Capacity {
                slots: 1 << 16,
                intake_per_tick: 4096,
                ack_backlog: 1 << 14,
                pending_write_backlog: 1 << 14,
                log_events: 1 << 12,
            },
            batching: BatchPolicy {
                size: 1_000,
                max: 10_000,
                queued: 10_000,
                linger: Duration::from_micros(200),
                in_flight: 8,
            },
            linked: LinkedPolicy { max_legs: 64 },
            safety: SafetyPolicy {
                quarantine_fail_stop: 3,
            },
            profile: false,
        }
    }
}

impl ReactorConfig {
    /// Refuses combinations that would misbehave silently.
    pub fn validate(&self) -> Result<(), LedgerError> {
        let sane = self.capacity.slots > 0
            && self.capacity.intake_per_tick > 0
            && self.capacity.log_events > 0
            && self.batching.size > 0
            && self.batching.in_flight > 0
            && self.batching.max >= self.batching.size
            // A batch that cannot be filled would never be proposed on fullness, only on linger.
            && self.batching.queued >= self.batching.max
            && self.linked.max_legs > 0
            && self.linked.max_legs <= self.batching.max
            && self.capacity.ack_backlog > 0
            && self.capacity.pending_write_backlog > 0
            && self.safety.quarantine_fail_stop > 0;
        if sane {
            Ok(())
        } else {
            Err(LedgerError::ConfigInvalid)
        }
    }

    /// Room for everything allowed to wait, twice over, plus one chain.
    ///
    /// Intake pauses at `queued`, but requests already dispatched are still judged after the pause, so
    /// the open batch overshoots by whatever was in flight at that moment. That overshoot is bounded
    /// only by the slot pool, and reserving for the pool would be tens of megabytes per buffer — so the
    /// reserve is a compromise, stated here as one factor rather than hidden in a formula: the work in
    /// flight when intake stops is of the same order as the bound that stopped it. Whether it held is
    /// not assumed — a sizing report prints the open batch's peak against `queued`, and a fill above
    /// one means this reserve was too small for that configuration.
    pub const fn batch_headroom(&self) -> usize {
        self.batching.queued * 2 + self.linked.max_legs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch ceiling below the longest chain could not hold one, so consensus would be handed a
    /// split chain. The config must refuse that instead of discovering it at runtime.
    #[test]
    fn a_batch_ceiling_that_cannot_hold_a_chain_is_refused() {
        assert_eq!(ReactorConfig::default().validate(), Ok(()));

        let split = ReactorConfig {
            batching: BatchPolicy {
                max: 8,
                ..ReactorConfig::default().batching
            },
            ..ReactorConfig::default()
        };
        assert!(split.batching.max < split.linked.max_legs);
        assert_eq!(split.validate(), Err(LedgerError::ConfigInvalid));
    }
}
