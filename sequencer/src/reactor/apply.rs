use ledger_base::ports::{AccountPort, IdempotencyPort, PendingEffect, PendingPort, RaftPort};
use ledger_base::Clock;

use super::Reactor;
use ledger_base::ports::RaftOutcome;
use ledger_base::{AckOutcome, Effect, EffectKind, LedgerError};

use crate::log_kind::LogKind;
use crate::state::pipeline::SlotId;

impl<A, P, I, R, C> Reactor<A, P, I, R, C>
where
    A: AccountPort,
    P: PendingPort,
    I: IdempotencyPort,
    R: RaftPort,
    C: Clock,
{
    /// S4: hand a due batch to consensus without waiting for it.
    pub(super) fn propose(&mut self) -> bool {
        let now = self.clock.now_nanos();
        let Some((proposal, slots)) = self.batcher.take_proposal(now) else {
            return false;
        };
        let batch_id = proposal.batch_id;
        match self.raft.propose(proposal) {
            Ok(()) => {
                self.batcher.on_proposed(batch_id, slots, now);
                self.metrics.proposed_batches += 1;
                true
            }
            Err(proposal) => {
                self.batcher.restore(proposal.effects, slots);
                self.metrics.propose_deferred += 1;
                false
            }
        }
    }

    /// S5: apply in commit order. This is the one stage that cannot be moved off the core.
    pub(super) fn apply(&mut self) -> bool {
        if self.safety.applies_sealed() {
            return false;
        }
        let mut progress = false;
        while let Some(commit) = self.raft.poll() {
            progress = true;
            let Some(batch) = self.batcher.next_committed() else {
                // An answer for a batch nobody is waiting for: the bookkeeping is off.
                self.seal_applies(0, commit.batch_id);
                break;
            };
            self.note_commit_wait(batch.proposed_at_nanos);
            if batch.batch_id != commit.batch_id {
                self.seal_applies(batch.batch_id, commit.batch_id);
                break;
            }
            match commit.outcome {
                RaftOutcome::Committed => self.commit_batch(&commit.effects, &batch.slots),
                RaftOutcome::Failed => {
                    self.record(
                        LogKind::COMMIT_FAILED,
                        commit.batch_id,
                        commit.effects.len() as u64,
                    );
                    self.roll_back_batch(&commit.effects, &batch.slots);
                }
            }
            self.batcher.recycle(commit.effects, batch.slots);
            if self.safety.applies_sealed() {
                break;
            }
        }
        progress
    }

    /// The effects of one batch and the slots of another cannot be paired: applying them would ack
    /// the wrong requests and release slots other requests still hold. So nothing more is applied,
    /// nothing more is answered, and the drain that never completes is the signal to replace this
    /// leader.
    fn seal_applies(&mut self, waiting: u64, answered: u64) {
        if self.safety.seal_applies() {
            self.record(LogKind::COMMIT_OUT_OF_ORDER, waiting, answered);
        }
    }

    /// What consensus cost this batch. One clock read per batch, not per request.
    fn note_commit_wait(&mut self, proposed_at_nanos: u64) {
        let waited = self.clock.now_nanos().saturating_sub(proposed_at_nanos);
        self.metrics.commit_wait_nanos += waited;
        self.metrics.commit_wait_max_nanos = self.metrics.commit_wait_max_nanos.max(waited);
    }

    pub(super) fn commit_batch(&mut self, effects: &[Effect], slots: &[SlotId]) {
        for (effect, &slot) in effects.iter().zip(slots) {
            if let Err(err) = self.accounts.apply(effect) {
                // Consensus committed this effect, so the ledger owes it: not applying it means the
                // state no longer follows the log. There is nothing to answer and nothing to skip —
                // a follower replaying the same log stops in the same place, and this node has to
                // be replaced.
                if self.safety.seal_applies() {
                    self.record(LogKind::APPLY_FAILED, self.metrics.committed, err.name().len() as u64);
                }
                return;
            }
            self.settle_overlays(effect);
            if let Some(write) = effect.pending_effect() {
                match write {
                    PendingEffect::Create { .. } => self.metrics.pending_creates += 1,
                    PendingEffect::Reduce { .. } => self.metrics.pending_reduces += 1,
                    PendingEffect::Remove { .. } => self.metrics.pending_removes += 1,
                }
                self.pending.write(write);
            }
            self.metrics.committed += 1;
            self.finish(slot, AckOutcome::Committed);
        }
    }

    pub(super) fn roll_back_batch(&mut self, effects: &[Effect], slots: &[SlotId]) {
        for (effect, &slot) in effects.iter().zip(slots) {
            self.give_back_overlays(effect);
            self.metrics.commit_failures += 1;
            self.finish(slot, AckOutcome::Rejected(LedgerError::RaftCommitFailed));
        }
    }

    pub(super) fn evict_idle_holds(&mut self) {
        let evicted = self.pending.maintain();
        if evicted == 0 {
            return;
        }
        self.metrics.holds_evicted += evicted as u64;
        let held = self.pending.overlay_len() as u64;
        self.record(LogKind::HOLDS_EVICTED, evicted as u64, held);
    }

    /// Only a resolution touches a hold's remainder, and only the engine holds that state.
    pub(super) fn reserve_hold(&mut self, effect: &Effect) {
        if matches!(effect.kind, EffectKind::Settle | EffectKind::Void) {
            self.pending.reserve(
                effect.pending_ref,
                effect.amount,
                effect.remaining_after == 0,
            );
        }
    }

    pub(super) fn compensate_hold(&mut self, effect: &Effect) {
        if matches!(effect.kind, EffectKind::Settle | EffectKind::Void) {
            self.pending.compensate(
                effect.pending_ref,
                effect.amount,
                effect.remaining_after == 0,
            );
        }
    }
}
