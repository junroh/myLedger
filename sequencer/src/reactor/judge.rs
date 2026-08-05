use ledger_base::ports::{AccountPort, IdempotencyPort, PendingPort, RaftPort};
use ledger_base::Clock;

use super::Reactor;
use ledger_base::ports::{HoldView, IdemReply, IdemVerdict, PendingReply};
use ledger_base::{
    AckOutcome, Amount, Effect, EffectKind, LedgerError, TransferKind, TxId, UNORDERED,
};

use crate::log_kind::LogKind;
use crate::rules::budget::{BudgetCoverage, BudgetRules};
use crate::rules::linked::{LinkedChain, LinkedScratch};
use crate::state::pipeline::{DepFlags, SlotId, WorkItem};

impl<A, P, I, R, C> Reactor<A, P, I, R, C>
where
    A: AccountPort,
    P: PendingPort,
    I: IdempotencyPort,
    R: RaftPort,
    C: Clock,
{
    /// S3: drain each external path independently, so a slow path never delays a fast one.
    pub fn drain_replies(&mut self) -> bool {
        let mut progress = false;
        while let Some(reply) = self.idem.poll() {
            progress = true;
            self.on_idem(reply);
        }
        while let Some(reply) = self.pending.poll() {
            progress = true;
            self.on_pending(reply);
        }
        progress
    }

    pub fn on_idem(&mut self, reply: IdemReply) {
        let slot = reply.correlation.raw();
        let judgeable = {
            let item = self.pipeline.item_mut(slot);
            item.verdict = Some(reply.verdict);
            item.clear_dep(DepFlags::IDEM);
            item.is_judgeable()
        };
        if judgeable {
            self.on_ready(slot);
        }
    }

    pub fn on_pending(&mut self, reply: PendingReply) {
        let slot = reply.correlation.raw();
        let item = *self.pipeline.item(slot);
        if item.keeps_lane_place() {
            self.lanes.get_mut(item.debit).pending_reply_arrived();
        }
        // The engine answered from state older than a decision it had already been handed. For a request
        // that keeps a place in its lane the seq check would have caught a reordering; for one that keeps
        // none — a resolution on an unconstrained account — this is the check that replaces it, and it is
        // stronger: it is about the data rather than the order in which it arrived.
        if reply.applied < self.pipeline.expected_applies(slot) {
            self.on_stale_answer(item.lane, item.debit);
        }
        if reply.pending_ref.is_absent() {
            self.release_dep(slot, DepFlags::PENDING);
            return;
        }
        // The record goes to the request that asked for it, and the engine is told the answer arrived so
        // its own state stops saying a lookup is on the way. The record is not kept anywhere else: the
        // engine owns the copy that outlives this request.
        self.pipeline.set_record(slot, reply.found);
        self.pending
            .admit_lookup(reply.pending_ref, reply.found.map(|found| found.remaining));
        self.release_dep(slot, DepFlags::PENDING);
    }

    /// A standalone request is judged as soon as its results are in; a linked leg waits for
    /// its whole chain, because the chain is judged and proposed as one unit.
    pub fn on_ready(&mut self, slot: SlotId) {
        let chain = self.pipeline.item(slot).chain;
        if chain.is_absent() {
            return self.judge(slot);
        }
        if let Some(ready) = self.linked.leg_ready(chain) {
            self.judge_chain(ready);
        }
    }

    fn release_dep(&mut self, slot: SlotId, dep: DepFlags) {
        let judgeable = {
            let item = self.pipeline.item_mut(slot);
            item.clear_dep(dep);
            item.is_judgeable()
        };
        if judgeable {
            self.on_ready(slot);
        }
    }

    pub(super) fn judge(&mut self, slot: SlotId) {
        let item = *self.pipeline.item(slot);
        match self
            .accept(&item)
            .and_then(|_| self.build_effect(slot, &item, None))
        {
            Ok(effect) => {
                self.take_overlays(&effect);
                let now = self.clock.now_nanos();
                self.batcher.push(effect, slot, now);
                self.metrics.judged += 1;
            }
            Err(err) => self.finish(slot, Self::rejection(err)),
        }
    }

    pub fn judge_chain(&mut self, chain: LinkedChain) {
        let (mut effects, mut scratch) = self.linked.take_buffers();
        let mut budgets = std::mem::replace(&mut self.budgets, BudgetCoverage::new(0));
        budgets.clear();

        let mut failure = chain.failure();
        for &slot in &chain.legs {
            let item = *self.pipeline.item(slot);
            // Every leg consumes its lane seq even once the chain is doomed, otherwise the
            // lane would show a gap for the requests behind it.
            let outcome = self
                .accept(&item)
                .and_then(|_| self.build_effect(slot, &item, Some(&scratch)));
            match (failure, outcome) {
                (None, Ok(effect)) => {
                    self.hold_this_chain_decided(slot, &effect, &mut budgets, &mut scratch);
                    effects.push(effect);
                }
                (None, Err(err)) => failure = Some(err),
                (Some(_), _) => {}
            }
        }
        // A shared budget group is resolved whole: every member, for its whole remainder.
        if failure.is_none() && budgets.misses_a_member() {
            failure = Some(LedgerError::SharedBudgetGroupIncomplete);
        }

        match failure {
            Some(err) => self.reject_chain(&chain, &mut effects, err),
            None => self.propose_chain(&chain, &mut effects),
        }
        self.linked.return_buffers(effects, scratch);
        self.budgets = budgets;
        self.open_gates(&chain);
        self.linked.recycle(chain);
    }

    /// One leg has been decided: the overlays take what it will spend, the coverage tally sees what
    /// it resolves of a budget group, and the scratch shows it to the legs behind it.
    fn hold_this_chain_decided(
        &mut self,
        slot: SlotId,
        effect: &Effect,
        budgets: &mut BudgetCoverage,
        scratch: &mut LinkedScratch,
    ) {
        if !effect.budget.is_absent() {
            if let Some(record) = self.pipeline.record(slot) {
                budgets.note(
                    effect.budget,
                    effect.amount,
                    record.budget_members,
                    record.budget_remaining,
                );
            }
        }
        self.take_overlays(effect);
        scratch.note(effect);
    }

    /// Whole or not at all: every leg is answered with the same error and nothing reaches a batch.
    fn reject_chain(&mut self, chain: &LinkedChain, effects: &mut Vec<Effect>, err: LedgerError) {
        for effect in effects.drain(..) {
            self.give_back_overlays(&effect);
        }
        for &slot in &chain.legs {
            self.finish(slot, Self::rejection(err));
        }
        self.metrics.linked_chains_rejected += 1;
        // A chain the client never terminated is a protocol violation, not a normal rejection, so
        // it is worth a log line.
        if err == LedgerError::LinkedChainUnterminated {
            self.metrics.linked_chains_aborted += 1;
            let legs = chain.legs.len() as u64;
            self.record(LogKind::CHAIN_ABORTED, chain.id.raw() as u64, legs);
        }
    }

    /// The legs enter the batch together, which is what keeps consensus from splitting them.
    fn propose_chain(&mut self, chain: &LinkedChain, effects: &mut Vec<Effect>) {
        let now = self.clock.now_nanos();
        for (effect, &slot) in effects.drain(..).zip(chain.legs.iter()) {
            self.batcher.push(effect, slot, now);
            self.metrics.judged += 1;
        }
        self.metrics.linked_chains_judged += 1;
    }

    pub(super) fn open_gates(&mut self, chain: &LinkedChain) {
        self.linked.open_gates(chain.id);
        for &slot in &chain.gated {
            self.release_dep(slot, DepFlags::LINKED_CHAIN);
        }
    }

    /// Checks that apply to every request before its effect is built.
    pub(super) fn accept(&mut self, item: &WorkItem) -> Result<(), LedgerError> {
        if self.safety.is_fail_stopped() {
            return Err(LedgerError::FailStop);
        }
        if self.lanes.get(item.debit).is_quarantined() {
            return Err(LedgerError::AccountQuarantined(item.lane));
        }
        if item.seq != UNORDERED {
            if let Err(err) = self.lanes.get_mut(item.debit).accept_seq(item.seq) {
                self.on_seq_gap(item.lane, item.debit, item.seq);
                return Err(err);
            }
        }
        match item.verdict {
            Some(IdemVerdict::DuplicateSameBody) => Err(LedgerError::DuplicateRequest),
            Some(IdemVerdict::DuplicateDifferentBody) => Err(LedgerError::DuplicateDifferentBody),
            _ => Ok(()),
        }
    }

    /// A duplicate is a fact, not a failure, so it gets its own ack.
    pub(super) fn rejection(error: LedgerError) -> AckOutcome {
        match error {
            LedgerError::DuplicateRequest => AckOutcome::Duplicate,
            other => AckOutcome::Rejected(other),
        }
    }

    /// `chain` is the scratch of the chain being judged, which is what makes a leg see what
    /// earlier legs of the same chain decided.
    pub(super) fn build_effect(
        &self,
        slot: SlotId,
        item: &WorkItem,
        chain: Option<&LinkedScratch>,
    ) -> Result<Effect, LedgerError> {
        let extra = chain.map_or(0, |scratch| scratch.available_for(item.debit));
        match item.kind {
            TransferKind::SinglePhase => self.direct_effect(item, EffectKind::Post, extra),
            TransferKind::Hold => self.direct_effect(item, EffectKind::Hold, extra),
            TransferKind::Settle => self.resolving_effect(slot, item, EffectKind::Settle, chain),
            TransferKind::Void => self.resolving_effect(slot, item, EffectKind::Void, chain),
        }
    }

    /// The hold a resolution is judged by, from the two places it comes from: the record the engine
    /// answered this very request with, and what the sequencer has decided about that hold since. Rule
    /// 18 is why it is not one place — the record is the engine's, and a second copy here would be the
    /// same fact under two owners.
    fn hold_view(&self, slot: SlotId, hold: TxId) -> Option<HoldView> {
        let record = self.pipeline.record(slot)?;
        Some(HoldView::compose(record, self.pending.overlay(hold)))
    }

    pub(super) fn direct_effect(
        &self,
        item: &WorkItem,
        kind: EffectKind,
        extra: Amount,
    ) -> Result<Effect, LedgerError> {
        // The balance decision is the sequencer's: the account component supplies committed
        // columns, the lane supplies the overlay, the chain supplies its scratch.
        let record = self.accounts.record(item.debit);
        let available = record.available() + self.lanes.get(item.debit).speculative() + extra;
        if record.is_constrained() && available < item.tx.amount {
            return Err(LedgerError::InsufficientBalance {
                available,
                requested: item.tx.amount,
            });
        }
        Ok(Effect {
            tx_id: item.tx.id,
            pending_ref: TxId::ABSENT,
            debit_account: item.tx.debit_account,
            credit_account: item.tx.credit_account,
            amount: item.tx.amount,
            remaining_after: 0,
            debit: item.debit,
            credit: item.credit,
            chain: item.chain,
            // The client declares the group a hold joins; nothing here invents one.
            budget: item.tx.budget(),
            ledger: item.tx.ledger,
            kind,
        })
    }

    /// Settle and void share every check; only the amount they consume differs.
    pub(super) fn resolving_effect(
        &self,
        slot: SlotId,
        item: &WorkItem,
        kind: EffectKind,
        chain: Option<&LinkedScratch>,
    ) -> Result<Effect, LedgerError> {
        let hold = chain
            .and_then(|scratch| scratch.hold(item.tx.pending_ref))
            .or_else(|| self.hold_view(slot, item.tx.pending_ref))
            .ok_or(LedgerError::PendingRefNotFound(item.tx.pending_ref))?;
        if hold.resolved {
            return Err(LedgerError::PendingRefAlreadyResolved);
        }
        if hold.debit_account != item.tx.debit_account
            || hold.credit_account != item.tx.credit_account
        {
            return Err(LedgerError::PendingRefMismatch);
        }
        if hold.ledger != item.tx.ledger {
            return Err(LedgerError::LedgerMismatch);
        }
        let remaining = hold.remaining;
        let amount = match kind {
            EffectKind::Void => remaining,
            _ => item.tx.amount,
        };
        if amount <= 0 {
            return Err(LedgerError::InvalidAmount);
        }
        if amount > remaining {
            return Err(LedgerError::SettleExceedsRemaining {
                remaining,
                requested: amount,
            });
        }
        BudgetRules::allow_resolution(&hold, amount, !item.chain.is_absent())?;
        // Handles are the sequencer's own index; the engine only knows account ids.
        let debit = self
            .accounts
            .resolve(hold.debit_account)
            .ok_or(LedgerError::UnknownAccount(hold.debit_account))?;
        let credit = self
            .accounts
            .resolve(hold.credit_account)
            .ok_or(LedgerError::UnknownAccount(hold.credit_account))?;
        Ok(Effect {
            tx_id: item.tx.id,
            pending_ref: item.tx.pending_ref,
            debit_account: hold.debit_account,
            credit_account: hold.credit_account,
            amount,
            remaining_after: remaining - amount,
            debit,
            credit,
            chain: item.chain,
            budget: hold.budget,
            ledger: hold.ledger,
            kind,
        })
    }
}
