//! Linked transfers: the legs of one submission commit or roll back together. Atomicity, lasting
//! one judgment and one proposal. Not [`crate::rules::budget`], which is a property of holds.

use ledger_base::ports::HoldView;
use ledger_base::{
    AcctHandle, Amount, Effect, EffectKind, FxHashMap, LedgerError, LinkedChainId, TxId,
};

use ledger_base::BufferPool;

use crate::state::pipeline::SlotId;

/// A run of consecutive requests: every leg but the last carries the linked flag. Judged as one
/// unit, which is what lets a later leg spend what an earlier one brings in.
pub struct LinkedChain {
    pub id: LinkedChainId,
    pub legs: Vec<SlotId>,
    /// Requests behind this chain on one of its lanes. The barrier is a wait the
    /// sequencer creates itself, so the sequencer also has to keep lane order across it.
    pub gated: Vec<SlotId>,
    outstanding: usize,
    closed: bool,
    failure: Option<LedgerError>,
}

impl LinkedChain {
    pub fn failure(&self) -> Option<LedgerError> {
        self.failure
    }

    fn is_complete(&self) -> bool {
        self.closed && self.outstanding == 0
    }
}

/// What exists only inside the chain being judged: availability an earlier leg brings in, and holds
/// an earlier leg creates.
///
/// This is not a general third layer, and there deliberately is none. The speculative overlay
/// records only availability-*reducing* deltas: money a request will bring in is never lent to
/// anyone else before it commits, because that request may still fail. Inside an atomic chain the
/// opposite is required — a later leg spends what an earlier leg brings in — and that is safe
/// precisely because the chain commits or rolls back as one. So the positive side lives here, for
/// the length of one chain judgment, and is discarded before anything else can see it.
///
/// LinkedChains are small, so a linear scan beats hashing.
pub struct LinkedScratch {
    gains: Vec<(AcctHandle, Amount)>,
    /// A hold this chain creates is not in the engine's overlay yet — the engine learns of it when
    /// the batch commits. It is visible here and nowhere else, so a resolution of it can only be
    /// judged inside the chain that creates it. Outside the chain the same visibility would be
    /// unsafe: a resolution in a later batch could commit after the batch that created the hold
    /// was refused, leaving a settle against a hold that never existed.
    holds: Vec<(TxId, HoldView)>,
}

impl LinkedScratch {
    pub fn new(capacity: usize) -> Self {
        Self {
            gains: Vec::with_capacity(capacity),
            holds: Vec::with_capacity(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.gains.clear();
        self.holds.clear();
    }

    /// Called for every effect this chain has decided, so later legs see what earlier ones did.
    pub fn note(&mut self, effect: &Effect) {
        self.add(effect.gain());
        match effect.kind {
            // A hold in a budget group is left out: the group's membership and remainder are the
            // engine's to report, and judging a group needs both.
            EffectKind::Hold if effect.budget.is_absent() => self.holds.push((
                effect.tx_id,
                HoldView {
                    debit_account: effect.debit_account,
                    credit_account: effect.credit_account,
                    ledger: effect.ledger,
                    budget: effect.budget,
                    budget_members: 0,
                    budget_remaining: 0,
                    remaining: effect.amount,
                    resolved: false,
                },
            )),
            EffectKind::Settle | EffectKind::Void => self.resolve(effect),
            _ => {}
        }
    }

    /// A hold this chain created, if this chain created it.
    pub fn hold(&self, hold: TxId) -> Option<HoldView> {
        self.holds
            .iter()
            .find(|(id, _)| *id == hold)
            .map(|(_, view)| *view)
    }

    fn resolve(&mut self, effect: &Effect) {
        if let Some((_, view)) = self
            .holds
            .iter_mut()
            .find(|(id, _)| *id == effect.pending_ref)
        {
            view.remaining = effect.remaining_after;
            view.resolved = effect.remaining_after == 0;
        }
    }

    pub fn add(&mut self, gain: Option<(AcctHandle, Amount)>) {
        if let Some((handle, amount)) = gain {
            self.gains.push((handle, amount));
        }
    }

    pub fn available_for(&self, handle: AcctHandle) -> Amount {
        self.gains
            .iter()
            .filter(|(gained, _)| *gained == handle)
            .map(|(_, amount)| *amount)
            .sum()
    }
}

pub struct LinkedChains {
    open: Option<LinkedChain>,
    closed: FxHashMap<LinkedChainId, LinkedChain>,
    gates: FxHashMap<AcctHandle, LinkedChainId>,
    effects: Vec<Effect>,
    scratch: LinkedScratch,
    max_legs: usize,
    next_id: u32,
    leg_buffers: BufferPool<SlotId>,
}

impl LinkedChains {
    pub fn new(max_legs: usize, chains_in_flight: usize) -> Self {
        Self {
            open: None,
            closed: FxHashMap::with_capacity_and_hasher(chains_in_flight, Default::default()),
            gates: FxHashMap::with_capacity_and_hasher(chains_in_flight, Default::default()),
            max_legs,
            next_id: 1,
            effects: Vec::with_capacity(max_legs),
            scratch: LinkedScratch::new(max_legs),
            leg_buffers: BufferPool::new(chains_in_flight * 2, max_legs),
        }
    }

    /// Called for every request in stream order. Returns the chain the request belongs to,
    /// which is `None` for an ordinary standalone transfer. A request without the linked
    /// flag terminates an open chain and is its last leg.
    pub fn admit(&mut self, linked: bool) -> Option<LinkedChainId> {
        if !linked {
            let mut chain = self.open.take()?;
            chain.closed = true;
            let id = chain.id;
            self.closed.insert(id, chain);
            return Some(id);
        }
        if self.open.is_none() {
            let id = LinkedChainId(self.next_id);
            self.next_id += 1;
            self.open = Some(LinkedChain {
                id,
                legs: self.leg_buffers.take(),
                gated: self.leg_buffers.take(),
                outstanding: 0,
                closed: false,
                failure: None,
            });
        }
        let chain = self.open.as_mut()?;
        if chain.legs.len() >= self.max_legs {
            chain.failure = chain.failure.or(Some(LedgerError::LinkedChainTooLong));
        }
        Some(chain.id)
    }

    pub fn add_leg(&mut self, id: LinkedChainId, lane: AcctHandle, slot: SlotId) {
        if let Some(chain) = self.chain_mut(id) {
            chain.legs.push(slot);
            chain.outstanding += 1;
        }
        self.gates.insert(lane, id);
    }

    /// The chain, if any, that a new request on this lane must be judged after.
    pub fn gate_for(&self, lane: AcctHandle) -> Option<LinkedChainId> {
        if self.gates.is_empty() {
            return None;
        }
        self.gates.get(&lane).copied()
    }

    pub fn wait_behind(&mut self, id: LinkedChainId, slot: SlotId) {
        if let Some(chain) = self.chain_mut(id) {
            chain.gated.push(slot);
        }
    }

    /// A leg that never reached the pipeline still dooms its chain: atomicity is the point.
    pub fn fail(&mut self, id: LinkedChainId, error: LedgerError) {
        if let Some(chain) = self.chain_mut(id) {
            chain.failure = chain.failure.or(Some(error));
        }
    }

    /// Returns the chain once its last leg has its external results.
    pub fn leg_ready(&mut self, id: LinkedChainId) -> Option<LinkedChain> {
        let chain = self.chain_mut(id)?;
        chain.outstanding -= 1;
        if !chain.is_complete() {
            return None;
        }
        self.closed.remove(&id)
    }

    /// A leg that never entered the pipeline still leaves the chain waiting, so completion is
    /// re-checked after such a failure.
    pub fn settle_if_complete(&mut self, id: LinkedChainId) -> Option<LinkedChain> {
        if !self.chain_mut(id)?.is_complete() {
            return None;
        }
        self.closed.remove(&id)
    }

    /// A chain still open at a batch boundary was abandoned, so it is rejected rather than left
    /// gating its lanes.
    pub fn close_batch(&mut self) -> Option<LinkedChain> {
        let mut chain = self.open.take()?;
        chain.closed = true;
        chain.failure = chain.failure.or(Some(LedgerError::LinkedChainUnterminated));
        if chain.is_complete() {
            return Some(chain);
        }
        let id = chain.id;
        self.closed.insert(id, chain);
        None
    }

    /// Scratch buffers for judging one chain, lent out so the reactor holds no chain state.
    pub fn take_buffers(&mut self) -> (Vec<Effect>, LinkedScratch) {
        let effects = std::mem::take(&mut self.effects);
        let mut scratch = std::mem::replace(&mut self.scratch, LinkedScratch::new(0));
        scratch.clear();
        (effects, scratch)
    }

    pub fn return_buffers(&mut self, effects: Vec<Effect>, scratch: LinkedScratch) {
        self.effects = effects;
        self.scratch = scratch;
    }

    /// Called once a chain has been judged: its lanes are free again.
    pub fn open_gates(&mut self, id: LinkedChainId) {
        self.gates.retain(|_, gate| *gate != id);
    }

    pub fn recycle(&mut self, chain: LinkedChain) {
        self.leg_buffers.give(chain.legs);
        self.leg_buffers.give(chain.gated);
    }

    fn chain_mut(&mut self, id: LinkedChainId) -> Option<&mut LinkedChain> {
        match self.open.as_mut() {
            Some(open) if open.id == id => Some(open),
            _ => self.closed.get_mut(&id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of linked requests forms one chain, and the first request without the linked flag is
    /// its last leg. A standalone request belongs to no chain at all.
    #[test]
    fn the_first_unlinked_request_ends_the_chain() {
        let mut chains = LinkedChains::new(4, 2);
        let first = chains.admit(true).expect("a linked request opens a chain");
        assert_eq!(
            chains.admit(true),
            Some(first),
            "the same chain, still open"
        );
        assert_eq!(
            chains.admit(false),
            Some(first),
            "the terminator is the last leg"
        );

        assert_eq!(
            chains.admit(false),
            None,
            "a standalone request is not a chain"
        );
        let second = chains.admit(true).expect("the next chain");
        assert_ne!(second, first);
    }

    /// A chain still open when the batch ends was abandoned by the client. It is failed rather than
    /// left waiting for a terminator that will never arrive.
    #[test]
    fn a_chain_still_open_at_the_batch_boundary_fails() {
        let mut chains = LinkedChains::new(4, 2);
        let id = chains.admit(true).expect("chain");
        chains.add_leg(id, AcctHandle::new(0), 0);

        assert!(
            chains.close_batch().is_none(),
            "the leg is still out for its external results"
        );
        let chain = chains.leg_ready(id).expect("complete once the leg is back");
        assert_eq!(chain.failure(), Some(LedgerError::LinkedChainUnterminated));

        chains.open_gates(id);
        assert_eq!(
            chains.gate_for(AcctHandle::new(0)),
            None,
            "the lane is free again"
        );
    }
}
