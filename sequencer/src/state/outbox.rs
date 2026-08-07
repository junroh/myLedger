use std::collections::VecDeque;

use ledger_base::{Ack, Footprint, Peak, Producer};

/// One answer the client has not taken.
pub const ACK_BYTES: usize = std::mem::size_of::<Ack>();

/// Acks the client has not taken yet. The backlog is bounded on purpose: when it fills,
/// intake pauses so backpressure reaches the client instead of growing memory here.
pub struct Outbox {
    acks: Producer<Ack>,
    backlog: VecDeque<Ack>,
    limit: usize,
    peak: Peak,
}

impl Outbox {
    pub fn new(acks: Producer<Ack>, limit: usize, reserve: usize) -> Self {
        Self {
            acks,
            backlog: VecDeque::with_capacity(reserve),
            limit,
            peak: Peak::default(),
        }
    }

    /// Order is preserved: nothing overtakes a backlogged ack.
    pub fn emit(&mut self, ack: Ack) {
        if !self.backlog.is_empty() || self.acks.push(ack).is_err() {
            self.backlog.push_back(ack);
            self.peak.saw(self.backlog.len());
        }
    }

    /// Answers the client has not taken. Held here because a client that stops reading must become
    /// backpressure rather than memory, so the peak is what that bound was worth.
    pub fn footprint(&self, footprint: &mut Footprint) {
        footprint.buffer::<Ack>(
            "ack backlog",
            self.backlog.len(),
            self.backlog.capacity(),
            self.peak.entries(),
        );
    }

    pub fn flush(&mut self) -> bool {
        let mut progress = false;
        while let Some(ack) = self.backlog.front() {
            if self.acks.push(*ack).is_err() {
                break;
            }
            self.backlog.pop_front();
            progress = true;
        }
        progress
    }

    pub fn is_saturated(&self) -> bool {
        self.backlog.len() >= self.limit
    }

    pub fn depth(&self) -> usize {
        self.backlog.len()
    }
}
