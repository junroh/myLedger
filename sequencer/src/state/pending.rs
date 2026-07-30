use std::collections::VecDeque;

use ledger_base::ports::{
    HoldData, HoldView, PendingCommand, PendingEffect, PendingFence, PendingLookup, PendingPort,
    OverlayState, PendingReply,
};
use ledger_base::{Amount, Footprint, Peak, TxId};

/// The sequencer's end of the pending path: the port plus the committed decisions that have
/// not been handed over yet. Both live together because the ordering rule ties them: a queued
/// write must reach the store before any later lookup, or the lookup could still see a hold
/// the sequencer has already resolved.
pub struct PendingChannel<P: PendingPort> {
    port: P,
    writes: VecDeque<PendingEffect>,
    limit: usize,
    peak: Peak,
}

impl<P: PendingPort> PendingChannel<P> {
    pub fn new(port: P, limit: usize, reserve: usize) -> Self {
        Self { port, writes: VecDeque::with_capacity(reserve), limit, peak: Peak::default() }
    }

    pub fn lookup(&mut self, lookup: PendingLookup) -> bool {
        self.port.send(PendingCommand::Lookup(lookup)).is_ok()
    }

    pub fn fence(&mut self, fence: PendingFence) -> bool {
        self.port.send(PendingCommand::Fence(fence)).is_ok()
    }

    pub fn write(&mut self, effect: PendingEffect) {
        if !self.writes.is_empty() || self.port.send(PendingCommand::Apply(effect)).is_err() {
            self.writes.push_back(effect);
            self.peak.saw(self.writes.len());
        }
    }

    /// Committed decisions the engine has not taken yet. A backlog here is the engine being slower
    /// than the reactor commits, which is why its peak belongs beside the engine's own latency.
    pub fn footprint(&self, footprint: &mut Footprint) {
        footprint.buffer::<PendingEffect>(
            "queued engine writes",
            self.writes.len(),
            self.writes.capacity(),
            self.peak.entries(),
        );
    }

    pub fn flush(&mut self) -> bool {
        let mut progress = false;
        while let Some(write) = self.writes.front() {
            if self.port.send(PendingCommand::Apply(*write)).is_err() {
                break;
            }
            self.writes.pop_front();
            progress = true;
        }
        progress
    }

    /// While writes are queued no lookup may be sent, because the queue is what preserves
    /// their order.
    pub fn blocks_lookups(&self) -> bool {
        !self.writes.is_empty()
    }

    pub fn is_saturated(&self) -> bool {
        self.writes.len() >= self.limit
    }

    /// The component behind the channel, so whoever owns the run can ask it what it is holding.
    pub fn port(&self) -> &P {
        &self.port
    }

    pub fn depth(&self) -> usize {
        self.writes.len()
    }

    pub fn poll(&self) -> Option<PendingReply> {
        self.port.poll()
    }

    /// The overlay, read and moved inline. The state is the engine's; the sequencer only
    /// tells it what it decided.
    pub fn overlay_state(&self, hold: TxId) -> OverlayState {
        self.port.overlay_state(hold)
    }

    pub fn begin_lookup(&mut self, hold: TxId) {
        self.port.begin_lookup(hold);
    }

    pub fn admit_lookup(&mut self, hold: TxId, found: Option<HoldData>) {
        self.port.admit_lookup(hold, found);
    }

    pub fn pin(&mut self, hold: TxId) {
        self.port.pin(hold);
    }

    pub fn unpin(&mut self, hold: TxId) {
        self.port.unpin(hold);
    }

    pub fn view(&self, hold: TxId) -> Option<HoldView> {
        self.port.view(hold)
    }

    pub fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.port.reserve(hold, amount, resolves);
    }

    pub fn release_reservation(&mut self, hold: TxId, amount: Amount) {
        self.port.release_reservation(hold, amount);
    }

    pub fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        self.port.compensate(hold, amount, resolves);
    }

    pub fn maintain(&mut self) -> usize {
        self.port.maintain()
    }

    pub fn overlay_len(&self) -> usize {
        self.port.overlay_len()
    }
}
