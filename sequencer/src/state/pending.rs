use std::collections::VecDeque;

use ledger_base::ports::{
    ApplyIndex, OverlayState, PendingCommand, PendingEffect, PendingFence, PendingLookup,
    PendingNotice, PendingPort, PendingReply,
};
use ledger_base::{Amount, Footprint, Peak, TxId};

/// One committed decision waiting to be handed to the engine.
pub const PENDING_EFFECT_BYTES: usize = std::mem::size_of::<PendingEffect>();

/// The sequencer's end of the pending path: the port plus the committed decisions that have
/// not been handed over yet. Both live together because the ordering rule ties them: a queued
/// write must reach the store before any later lookup, or the lookup could still see a hold
/// the sequencer has already resolved.
pub struct PendingChannel<P: PendingPort> {
    port: P,
    /// Committed decisions the engine has not taken yet, each with the log position of its batch: a
    /// deferred write must not lose the position it belongs to.
    writes: VecDeque<(PendingEffect, ApplyIndex)>,
    limit: usize,
    peak: Peak,
    /// Committed decisions handed over. A lookup dispatched now is behind every one of them — the queue
    /// is one channel and a queued write blocks lookups — so an answer that reflects fewer is an engine
    /// that has reordered its own queue.
    applies_sent: u64,
    /// Expiry voids this sequencer would not take, waiting to be handed back. Retried like a write and
    /// for a weaker version of the same reason: a lost one does not lose money, but it leaves the sweep
    /// unable to tell a refused void from one still in flight, which is the state it used to be in for
    /// all of them.
    declines: VecDeque<TxId>,
}

impl<P: PendingPort> PendingChannel<P> {
    /// Tells the engine whether the sequencer has room for more expiry voids. One-way and advisory: the
    /// engine's sweep reads it before offering, so a full backlog stops the offers instead of declining them
    /// one at a time.
    pub fn set_wants_expiry(&mut self, wanted: bool) {
        self.port.set_wants_expiry(wanted);
    }

    pub fn new(port: P, limit: usize, reserve: usize) -> Self {
        Self {
            port,
            writes: VecDeque::with_capacity(reserve),
            limit,
            peak: Peak::default(),
            applies_sent: 0,
            declines: VecDeque::new(),
        }
    }

    /// Hands back an expiry void this sequencer refused, so the sweep retries that one and not the ones
    /// it is still waiting on.
    pub fn decline_expiry(&mut self, hold: TxId) {
        if !self.declines.is_empty()
            || self
                .port
                .send(PendingCommand::ExpiryDeclined { hold })
                .is_err()
        {
            self.declines.push_back(hold);
        }
    }

    /// Offers again whatever the engine would not take. Called where the writes are, because both are
    /// the same job: a command the queue refused is one this end still owes.
    pub fn drain_declines(&mut self) -> bool {
        let mut sent = false;
        while let Some(hold) = self.declines.front().copied() {
            if self
                .port
                .send(PendingCommand::ExpiryDeclined { hold })
                .is_err()
            {
                break;
            }
            self.declines.pop_front();
            sent = true;
        }
        sent
    }

    pub fn lookup(&mut self, lookup: PendingLookup) -> bool {
        self.port.send(PendingCommand::Lookup(lookup)).is_ok()
    }

    pub fn fence(&mut self, fence: PendingFence) -> bool {
        self.port.send(PendingCommand::Fence(fence)).is_ok()
    }

    pub fn write(&mut self, effect: PendingEffect, at: ApplyIndex) {
        if !self.writes.is_empty()
            || self
                .port
                .send(PendingCommand::Apply { effect, at })
                .is_err()
        {
            self.writes.push_back((effect, at));
            self.peak.saw(self.writes.len());
            return;
        }
        self.applies_sent += 1;
    }

    /// Committed decisions the engine has been given. What a lookup dispatched now must be answered
    /// against.
    pub fn applies_sent(&self) -> u64 {
        self.applies_sent
    }

    /// Committed decisions the engine has not taken yet. A backlog here is the engine being slower
    /// than the reactor commits, which is why its peak belongs beside the engine's own latency.
    pub fn footprint(&self, footprint: &mut Footprint) {
        footprint.buffer::<PendingEffect>(
            "queued pending writes",
            self.writes.len(),
            self.writes.capacity(),
            self.peak.entries(),
        );
    }

    pub fn flush(&mut self) -> bool {
        let mut progress = false;
        while let Some(write) = self.writes.front() {
            let (effect, at) = *write;
            if self
                .port
                .send(PendingCommand::Apply { effect, at })
                .is_err()
            {
                break;
            }
            self.writes.pop_front();
            self.applies_sent += 1;
            progress = true;
        }
        progress | self.drain_declines()
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

    /// What the engine said without being asked. Its own channel, so it is neither behind a reply nor
    /// in front of one.
    pub fn notice(&self) -> Option<PendingNotice> {
        self.port.notices()
    }

    /// The inline half of the port, forwarded so the reactor reads one wrapper. Everything below this
    /// line runs on the reactor's own thread and cannot refuse; everything above it is queued and can,
    /// which is the difference the two contracts carry.
    pub fn hold_is_missing(&self, hold: TxId) -> bool {
        self.port.hold_is_missing(hold)
    }

    pub fn begin_lookup(&mut self, hold: TxId) {
        self.port.begin_lookup(hold);
    }

    pub fn admit_lookup(&mut self, hold: TxId, remaining: Option<Amount>) {
        self.port.admit_lookup(hold, remaining);
    }

    pub fn pin(&mut self, hold: TxId) {
        self.port.pin(hold);
    }

    pub fn unpin(&mut self, hold: TxId) {
        self.port.unpin(hold);
    }

    pub fn overlay(&self, hold: TxId) -> OverlayState {
        self.port.overlay(hold)
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
