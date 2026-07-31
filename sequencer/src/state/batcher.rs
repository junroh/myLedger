use std::collections::VecDeque;
use std::mem::size_of;

use ledger_base::ports::RaftProposal;
use ledger_base::{Effect, Footprint, Peak};

use crate::config::BatchPolicy;
use ledger_base::BufferPool;

use crate::state::pipeline::SlotId;

pub struct InFlightBatch {
    pub batch_id: u64,
    pub slots: Vec<SlotId>,
    /// When consensus was handed this batch, so apply can say what the round trip cost.
    pub proposed_at_nanos: u64,
}

/// S4 in one place: accumulate judged effects, decide when to propose, hand the buffers to
/// consensus and take them back for reuse.
pub struct Batcher {
    policy: BatchPolicy,
    effects: Vec<Effect>,
    slots: Vec<SlotId>,
    opened_at_nanos: Option<u64>,
    in_flight: VecDeque<InFlightBatch>,
    effect_buffers: BufferPool<Effect>,
    slot_buffers: BufferPool<SlotId>,
    next_batch_id: u64,
    /// What the open batch's buffer was reserved for. Kept so the reserve can be checked rather than
    /// trusted: it is a compromise, not a bound, and a push past it is the reallocation on the
    /// reactor's thread that nothing here is allowed to do.
    reserved: usize,
    open_peak: Peak,
    /// The largest the open batch's buffer ever grew. Tracked separately from the peak *length*
    /// because `detach` swaps the buffer for a pooled one: the buffer that reached the peak is often
    /// not the one held now, so reporting this buffer's capacity beside that peak would describe two
    /// different allocations as if they were one.
    open_bytes_peak: Peak,
    in_flight_peak: Peak,
}

impl Batcher {
    pub fn new(policy: BatchPolicy, headroom: usize) -> Self {
        Self {
            effects: Vec::with_capacity(headroom),
            slots: Vec::with_capacity(headroom),
            opened_at_nanos: None,
            in_flight: VecDeque::with_capacity(policy.in_flight),
            effect_buffers: BufferPool::new(policy.in_flight + 1, headroom),
            slot_buffers: BufferPool::new(policy.in_flight + 1, headroom),
            next_batch_id: 1,
            reserved: headroom,
            open_peak: Peak::default(),
            open_bytes_peak: Peak::default(),
            in_flight_peak: Peak::default(),
            policy,
        }
    }

    pub fn push(&mut self, effect: Effect, slot: SlotId, now_nanos: u64) {
        if self.effects.is_empty() {
            self.opened_at_nanos = Some(now_nanos);
        }
        // The reserve is a compromise between covering the overshoot past `queued` and not reserving
        // for the whole slot pool, so it is asserted rather than assumed. A release build counts on the
        // sizing report's fill ratio instead, which is why that ratio is printed.
        debug_assert!(
            self.effects.len() < self.reserved,
            "the open batch outgrew its reserve of {}: judged effects are reallocating on the \
             reactor's thread",
            self.reserved
        );
        self.effects.push(effect);
        self.slots.push(slot);
        self.open_peak.saw(self.effects.len());
        self.open_bytes_peak.saw(self.effects.capacity());
    }

    /// Whether judging has to stop offering work. Reaching the queued bound is backpressure, not a
    /// refusal: intake pauses, the client feels it, and the effects already here get proposed.
    pub fn is_saturated(&self) -> bool {
        self.effects.len() >= self.policy.queued
    }

    /// The batch being filled, the ones consensus still owes an answer for, and the spare buffers that
    /// keep the steady state allocation-free. This is the memory one round trip costs, and the pool is
    /// part of it: it is preallocated for exactly that reason, so it is always there.
    pub fn footprint(&self, footprint: &mut Footprint) {
        // Priced at the largest the buffer ever was, not at the one held now: a smaller one now means
        // the big one went back to the pool, not that the memory was never needed.
        footprint.other(
            "open batch effects",
            self.effects.len(),
            self.open_peak.entries(),
            self.policy.queued,
            self.open_bytes_peak.entries() * size_of::<Effect>(),
        );
        // Each batch in flight carries the slot list it will answer, which comes from the pool.
        let in_flight_slots: usize = self
            .in_flight
            .iter()
            .map(|batch| batch.slots.capacity())
            .sum();
        footprint.other(
            "batches awaiting consensus",
            self.in_flight.len(),
            self.in_flight_peak.entries(),
            // Consensus holding every proposal it may is the intended steady state, not a bound that
            // went wrong, so there is nothing here for a fill ratio to warn about.
            0,
            self.in_flight.capacity() * size_of::<InFlightBatch>()
                + in_flight_slots * size_of::<SlotId>(),
        );
        footprint.other(
            "spare batch buffers",
            self.effect_buffers.held() + self.slot_buffers.held(),
            0,
            0,
            self.effect_buffers.bytes() + self.slot_buffers.bytes(),
        );
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Takes the next proposal when one is due. The caller proposes it and then reports the
    /// outcome, so a full consensus queue costs nothing but a retry.
    pub fn take_proposal(&mut self, now_nanos: u64) -> Option<(RaftProposal, Vec<SlotId>)> {
        if self.effects.is_empty() || self.in_flight.len() >= self.policy.in_flight {
            return None;
        }
        let full = self.effects.len() >= self.policy.size;
        let lingered = self.opened_at_nanos.is_some_and(|opened| {
            now_nanos.saturating_sub(opened) >= self.policy.linger.as_nanos() as u64
        });
        if !full && !lingered {
            return None;
        }
        let take = self.chain_boundary(self.effects.len().min(self.policy.max));
        let (effects, slots) = self.detach(take);
        Some((
            RaftProposal {
                batch_id: self.next_batch_id,
                effects,
            },
            slots,
        ))
    }

    pub fn on_proposed(&mut self, batch_id: u64, slots: Vec<SlotId>, now_nanos: u64) {
        self.in_flight.push_back(InFlightBatch {
            batch_id,
            slots,
            proposed_at_nanos: now_nanos,
        });
        self.in_flight_peak.saw(self.in_flight.len());
        self.next_batch_id += 1;
        if self.effects.is_empty() {
            self.opened_at_nanos = None;
        }
    }

    /// Puts a refused proposal back at the front: those effects were judged first.
    pub fn restore(&mut self, effects: Vec<Effect>, slots: Vec<SlotId>) {
        if self.effects.is_empty() {
            let spare = std::mem::replace(&mut self.effects, effects);
            self.effect_buffers.give(spare);
            let spare = std::mem::replace(&mut self.slots, slots);
            self.slot_buffers.give(spare);
            return;
        }
        self.effects.splice(0..0, effects.iter().copied());
        self.slots.splice(0..0, slots.iter().copied());
        self.effect_buffers.give(effects);
        self.slot_buffers.give(slots);
    }

    pub fn next_committed(&mut self) -> Option<InFlightBatch> {
        self.in_flight.pop_front()
    }

    pub fn recycle(&mut self, effects: Vec<Effect>, slots: Vec<SlotId>) {
        self.effect_buffers.give(effects);
        self.slot_buffers.give(slots);
    }

    /// Walks back to the nearest chain boundary: a linked chain is atomic at the consensus level
    /// only if all of its legs stay inside one batch.
    fn chain_boundary(&self, mut take: usize) -> usize {
        while take > 0 && take < self.effects.len() {
            let before = self.effects[take - 1].chain;
            if before.is_absent() || before != self.effects[take].chain {
                break;
            }
            take -= 1;
        }
        take
    }

    fn detach(&mut self, take: usize) -> (Vec<Effect>, Vec<SlotId>) {
        if take == self.effects.len() {
            return (
                std::mem::replace(&mut self.effects, self.effect_buffers.take()),
                std::mem::replace(&mut self.slots, self.slot_buffers.take()),
            );
        }
        let mut effects = self.effect_buffers.take();
        let mut slots = self.slot_buffers.take();
        effects.extend(self.effects.drain(..take));
        slots.extend(self.slots.drain(..take));
        (effects, slots)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ledger_base::{AccountId, AcctHandle, EffectKind, LinkedChainId, TxId};

    use super::*;

    fn effect(chain: u32) -> Effect {
        Effect {
            tx_id: TxId(1),
            pending_ref: TxId::ABSENT,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 1,
            remaining_after: 0,
            debit: AcctHandle::new(0),
            credit: AcctHandle::new(1),
            chain: LinkedChainId(chain),
            budget: Default::default(),
            ledger: 1,
            kind: EffectKind::Post,
        }
    }

    /// `max` bounds what one proposal carries, which is a different thing from what may wait for one.
    /// While `in_flight` proposals are already outstanding no further one can be taken at all, so
    /// without `queued` the buffer of judged effects grew for as long as consensus was slow — an
    /// unbounded backlog because a peer is slow, which is the one thing every queue here forbids.
    #[test]
    fn every_proposal_in_flight_makes_judged_effects_saturate_rather_than_pile_up() {
        let policy = BatchPolicy {
            size: 1,
            max: 2,
            queued: 4,
            linger: Duration::ZERO,
            in_flight: 1,
        };
        let mut batcher = Batcher::new(policy, 8);
        // Leave consensus holding every proposal it may, so nothing more can be taken.
        batcher.push(effect(0), 0, 0);
        let (proposal, slots) = batcher.take_proposal(0).expect("a batch is due");
        batcher.on_proposed(proposal.batch_id, slots, 0);
        assert!(
            batcher.take_proposal(0).is_none(),
            "every proposal it may hold is in flight"
        );

        for slot in 1..=4 {
            assert!(
                !batcher.is_saturated(),
                "room for {slot} effects was promised"
            );
            batcher.push(effect(slot as u32), slot as SlotId, 0);
        }
        assert!(batcher.is_saturated(), "at the bound, intake has to pause");
    }

    /// The ceiling cuts a batch mid-chain, so the cut walks back to the chain boundary: the two
    /// legs travel together in the next proposal instead of being split across two.
    #[test]
    fn a_batch_is_cut_at_a_chain_boundary_not_inside_a_chain() {
        let policy = BatchPolicy {
            size: 1,
            max: 2,
            queued: 2,
            linger: Duration::ZERO,
            in_flight: 4,
        };
        let mut batcher = Batcher::new(policy, 8);
        for (index, chain) in [0, 7, 7].into_iter().enumerate() {
            batcher.push(effect(chain), index as SlotId, 0);
        }

        let (first, slots) = batcher.take_proposal(0).expect("a full batch is due");
        assert_eq!(
            first.effects.len(),
            1,
            "the chain's first leg would have been cut off"
        );
        batcher.on_proposed(first.batch_id, slots, 0);

        let (second, _) = batcher
            .take_proposal(0)
            .expect("the chain is still waiting");
        assert_eq!(second.effects.len(), 2);
        assert!(second
            .effects
            .iter()
            .all(|effect| effect.chain == LinkedChainId(7)));
    }
}
