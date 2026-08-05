use crate::effect::{ColumnDelta, Effect};
use crate::error::LedgerError;
use crate::ids::{AccountId, AcctHandle, Amount};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct AccountFlags(u8);

impl AccountFlags {
    pub const NONE: Self = Self(0);
    /// Debits are checked against available balance. Clearing and external accounts leave it off.
    pub const CONSTRAINED: Self = Self(1 << 0);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The durable per-account truth: four columns plus configuration. Owned by the account component,
/// which keeps it in DRAM and persists it on its own. Nothing about a request in flight lives here.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountRecord {
    debits_posted: Amount,
    credits_posted: Amount,
    debits_pending: Amount,
    credits_pending: Amount,
    ledger: u32,
    flags: AccountFlags,
}

// Measured: padding to 64 costs 60% more memory and runs twice as slow at a million accounts,
// because the footprint matters more than the straddling.
crate::layout_claim!(
    LAYOUT: AccountRecord,
    size = 40,
    crate::layout::LineFit::Straddles(crate::layout::ON_PURPOSE)
);

impl AccountRecord {
    pub const fn new(ledger: u32, flags: AccountFlags) -> Self {
        Self {
            debits_posted: 0,
            credits_posted: 0,
            debits_pending: 0,
            credits_pending: 0,
            ledger,
            flags,
        }
    }

    pub const fn ledger(&self) -> u32 {
        self.ledger
    }

    pub const fn is_constrained(&self) -> bool {
        self.flags.contains(AccountFlags::CONSTRAINED)
    }

    pub const fn debits_posted(&self) -> Amount {
        self.debits_posted
    }

    pub const fn credits_posted(&self) -> Amount {
        self.credits_posted
    }

    pub const fn debits_pending(&self) -> Amount {
        self.debits_pending
    }

    pub const fn credits_pending(&self) -> Amount {
        self.credits_pending
    }

    /// Committed availability. Pending credits are tracked but never spendable, or voiding an
    /// incoming hold would leave phantom funds behind.
    pub const fn available(&self) -> Amount {
        self.credits_posted - self.debits_posted - self.debits_pending
    }

    /// Whether this side can take the delta at all, and why not when it cannot. Asked before anything
    /// is written, because half an effect cannot be taken back: the batch is already committed. The
    /// debit side has no twin here — its own `apply_debit` computes both columns before assigning
    /// either, so it is its own check.
    pub const fn can_credit(&self, columns: ColumnDelta) -> Result<(), LedgerError> {
        if let Err(err) = Self::check(self.credits_posted, columns.posted) {
            return Err(err);
        }
        Self::check(self.credits_pending, columns.pending)
    }

    /// Both of this side's columns move or neither does.
    pub fn apply_debit(&mut self, columns: ColumnDelta) -> Result<(), LedgerError> {
        let posted = Self::sum(self.debits_posted, columns.posted)?;
        let pending = Self::sum(self.debits_pending, columns.pending)?;
        self.debits_posted = posted;
        self.debits_pending = pending;
        Ok(())
    }

    pub fn apply_credit(&mut self, columns: ColumnDelta) -> Result<(), LedgerError> {
        let posted = Self::sum(self.credits_posted, columns.posted)?;
        let pending = Self::sum(self.credits_pending, columns.pending)?;
        self.credits_posted = posted;
        self.credits_pending = pending;
        Ok(())
    }

    fn sum(value: Amount, delta: Amount) -> Result<Amount, LedgerError> {
        Self::check(value, delta)?;
        Ok(value + delta)
    }

    /// Both ways a column can be asked for something it cannot give. Below zero is the one that matters:
    /// a column is a running total of money reserved or moved, so a negative one is not a large number
    /// but a claim that the ledger released what it never held. Checked here, at the only place a column
    /// changes, rather than by walking every account — that walk is `audit`, and it runs between ticks.
    const fn check(value: Amount, delta: Amount) -> Result<(), LedgerError> {
        match value.checked_add(delta) {
            None => Err(LedgerError::BalanceOverflow),
            Some(total) if total < 0 => Err(LedgerError::ColumnWentNegative),
            Some(_) => Ok(()),
        }
    }
}

/// Both sums of double-entry bookkeeping. The sequencer reads them to check that what it decided
/// and what the account component applied still add up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerTotals {
    pub debits_posted: Amount,
    pub credits_posted: Amount,
    pub debits_pending: Amount,
    pub credits_pending: Amount,
}

impl LedgerTotals {
    pub const fn balanced(&self) -> bool {
        self.debits_posted == self.credits_posted && self.debits_pending == self.credits_pending
    }
}

/// The account component as the sequencer sees it. Calls are inline because the judge cannot
/// proceed without the answer; the component still owns the data and its persistence.
pub trait AccountPort {
    fn resolve(&self, id: AccountId) -> Option<AcctHandle>;
    fn record(&self, handle: AcctHandle) -> &AccountRecord;
    fn apply(&mut self, effect: &Effect) -> Result<(), LedgerError>;
    /// Effects applied so far, for reconciling this view with the others.
    fn applied(&self) -> u64;
    /// The four column sums. Walks every account, so it is for a check rather than for a tick.
    fn totals(&self) -> LedgerTotals;
}
