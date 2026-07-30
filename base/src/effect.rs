use crate::ids::{AccountId, AcctHandle, Amount, BudgetGroup, LinkedChainId, TxId};
use crate::ports::PendingEffect;

/// How one side's two columns move. Both sides of a transfer move the same way, which is
/// what makes the accounting identity structural.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColumnDelta {
    pub posted: Amount,
    pub pending: Amount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    Post,
    Hold,
    Settle,
    Void,
}

/// The replicated unit: what the leader decided, applied by every node without
/// re-running the decision. Handles are a leader-local shortcut; account ids are what
/// followers resolve against.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Effect {
    pub tx_id: TxId,
    pub pending_ref: TxId,
    pub debit_account: AccountId,
    pub credit_account: AccountId,
    pub amount: Amount,
    pub remaining_after: Amount,
    pub debit: AcctHandle,
    pub credit: AcctHandle,
    /// The linked chain this effect belongs to, so consensus never splits one.
    pub chain: LinkedChainId,
    /// The shared budget group the hold belongs to, carried for its whole life.
    pub budget: BudgetGroup,
    pub ledger: u32,
    pub kind: EffectKind,
}

// 112, not 96: the budget group is a client-supplied transaction id, so it costs 16 bytes in every
// replicated effect.
crate::layout_claim!(
    LAYOUT: Effect,
    size = 112,
    crate::layout::LineFit::Straddles(crate::layout::STREAMED)
);

impl Effect {
    /// The four-field delta table, in one place: how much of `amount` moves into the
    /// posted column and how much into the pending column, for both sides.
    pub const fn columns(&self) -> ColumnDelta {
        let amount = self.amount;
        match self.kind {
            EffectKind::Post => ColumnDelta {
                posted: amount,
                pending: 0,
            },
            EffectKind::Hold => ColumnDelta {
                posted: 0,
                pending: amount,
            },
            EffectKind::Settle => ColumnDelta {
                posted: amount,
                pending: -amount,
            },
            EffectKind::Void => ColumnDelta {
                posted: 0,
                pending: -amount,
            },
        }
    }

    /// Only availability-reducing deltas are held in the overlay before commit, so a
    /// failed commit can only cause a false reject, never an overdraft.
    pub const fn reservation(&self) -> Amount {
        match self.kind {
            EffectKind::Post | EffectKind::Hold => self.amount,
            EffectKind::Settle | EffectKind::Void => 0,
        }
    }

    /// Availability this effect adds once committed. Kept out of the overlay because a
    /// third party must not spend it before commit; the legs of an atomic chain may, since
    /// they commit or roll back with it.
    pub const fn gain(&self) -> Option<(AcctHandle, Amount)> {
        match self.kind {
            EffectKind::Post | EffectKind::Settle => Some((self.credit, self.amount)),
            EffectKind::Void => Some((self.debit, self.amount)),
            EffectKind::Hold => None,
        }
    }

    pub const fn pending_effect(&self) -> Option<PendingEffect> {
        match self.kind {
            EffectKind::Post => None,
            EffectKind::Hold => Some(PendingEffect::Create {
                tx_id: self.tx_id,
                debit_account: self.debit_account,
                credit_account: self.credit_account,
                amount: self.amount,
                ledger: self.ledger,
                budget: self.budget,
            }),
            EffectKind::Settle if self.remaining_after > 0 => Some(PendingEffect::Reduce {
                pending_ref: self.pending_ref,
                remaining: self.remaining_after,
            }),
            EffectKind::Settle | EffectKind::Void => Some(PendingEffect::Remove {
                pending_ref: self.pending_ref,
            }),
        }
    }
}
