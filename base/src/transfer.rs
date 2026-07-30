use crate::error::LedgerError;
use crate::ids::{AccountId, Amount, BudgetGroup, TxId, MAX_AMOUNT};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct TransferFlags(u16);

impl TransferFlags {
    pub const NONE: Self = Self(0);
    pub const PENDING: Self = Self(1 << 0);
    pub const POST_PENDING: Self = Self(1 << 1);
    pub const VOID_PENDING: Self = Self(1 << 2);
    pub const LINKED: Self = Self(1 << 3);

    const PHASE_MASK: u16 = 0b111;

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn raw(self) -> u16 {
        self.0
    }
}

impl core::ops::BitOr for TransferFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.with(rhs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferKind {
    #[default]
    SinglePhase,
    Hold,
    Settle,
    Void,
}

impl TransferKind {
    pub const fn needs_pending_lookup(self) -> bool {
        matches!(self, Self::Settle | Self::Void)
    }
}

/// One movement of money, always between exactly two accounts, which is what makes the
/// accounting identity structural rather than checked.
///
/// Field order is layout-significant: both 16-byte ids lead so `repr(C)` inserts no padding
/// between them and the 8-byte tail, keeping the transfer at one cache line.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct Transfer {
    /// Client-chosen identity, and the idempotency key: the same id twice is the same request.
    pub id: TxId,
    /// Depends on the kind. On a settle or void it names the hold being resolved; on a hold it
    /// names the shared budget group the hold joins, if any; on a single-phase transfer it must
    /// be absent.
    pub pending_ref: TxId,
    /// The paying side, and the lane whose order the ledger preserves. Balance constraints only
    /// ever apply here.
    pub debit_account: AccountId,
    /// The receiving side.
    pub credit_account: AccountId,
    /// Integer minor units, always positive. On a void it is ignored: a void releases whatever
    /// is left.
    pub amount: Amount,
    /// Both accounts must belong to this ledger; money never crosses ledgers implicitly.
    pub ledger: u32,
    /// Which of the four kinds this is, plus whether it is linked to the next request.
    pub flags: TransferFlags,
}

crate::layout_claim!(LAYOUT: Transfer, size = 64, crate::layout::LineFit::Straddles(crate::layout::STREAMED));

impl Transfer {
    /// Order lane. The debit side is the only side balance constraints apply to,
    /// so it is the lane whose seq order the external contract must preserve.
    pub const fn lane(&self) -> AccountId {
        self.debit_account
    }

    /// The shared budget group a hold joins. Other kinds use `pending_ref` for the hold they
    /// resolve, so they never declare one.
    pub const fn budget(&self) -> BudgetGroup {
        match self.kind() {
            Ok(TransferKind::Hold) => BudgetGroup(self.pending_ref.raw()),
            _ => BudgetGroup::ABSENT,
        }
    }

    pub const fn kind(&self) -> Result<TransferKind, LedgerError> {
        match self.flags.raw() & TransferFlags::PHASE_MASK {
            0 => Ok(TransferKind::SinglePhase),
            1 => Ok(TransferKind::Hold),
            2 => Ok(TransferKind::Settle),
            4 => Ok(TransferKind::Void),
            _ => Err(LedgerError::InvalidFlags),
        }
    }

    /// Shape-only validation: everything decidable without ledger state.
    pub const fn validate(&self) -> Result<TransferKind, LedgerError> {
        let kind = match self.kind() {
            Ok(kind) => kind,
            Err(err) => return Err(err),
        };
        if self.id.is_absent() || self.debit_account.is_absent() || self.credit_account.is_absent() {
            return Err(LedgerError::InvalidFlags);
        }
        if self.debit_account.raw() == self.credit_account.raw() {
            return Err(LedgerError::SameAccount);
        }
        match kind {
            TransferKind::SinglePhase => {
                if self.amount <= 0 || self.amount > MAX_AMOUNT {
                    return Err(LedgerError::InvalidAmount);
                }
                if !self.pending_ref.is_absent() {
                    return Err(LedgerError::UnexpectedPendingRef);
                }
            }
            // On a hold, `pending_ref` names the shared budget group it joins, if any.
            TransferKind::Hold => {
                if self.amount <= 0 || self.amount > MAX_AMOUNT {
                    return Err(LedgerError::InvalidAmount);
                }
            }
            TransferKind::Settle => {
                if self.amount <= 0 || self.amount > MAX_AMOUNT {
                    return Err(LedgerError::InvalidAmount);
                }
                if self.pending_ref.is_absent() {
                    return Err(LedgerError::MissingPendingRef);
                }
            }
            TransferKind::Void => {
                if self.pending_ref.is_absent() {
                    return Err(LedgerError::MissingPendingRef);
                }
            }
        }
        Ok(kind)
    }

    /// Body fingerprint for idempotency: same id with a different digest is a conflict.
    pub fn digest(&self) -> u64 {
        const MIX: u64 = 0x517c_c1b7_2722_0a95;
        let words = [
            self.id.raw() as u64,
            (self.id.raw() >> 64) as u64,
            self.debit_account.raw(),
            self.credit_account.raw(),
            self.amount as u64,
            self.pending_ref.raw() as u64,
            (self.pending_ref.raw() >> 64) as u64,
            (self.ledger as u64) << 16 | self.flags.raw() as u64,
        ];
        let mut hash = MIX;
        for word in words {
            hash = (hash.rotate_left(5) ^ word).wrapping_mul(MIX);
        }
        hash
    }
}
