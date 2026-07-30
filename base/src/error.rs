use crate::ids::{AccountId, Amount, Seq, TxId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerError {
    InvalidFlags,
    InvalidAmount,
    SameAccount,
    UnknownAccount(AccountId),
    LedgerMismatch,
    MissingPendingRef,
    UnexpectedPendingRef,
    SeqGap { expected: Seq, got: Seq },
    AccountQuarantined(AccountId),
    FailStop,
    InsufficientBalance { available: Amount, requested: Amount },
    PendingRefNotFound(TxId),
    PendingRefMismatch,
    PendingRefAlreadyResolved,
    SettleExceedsRemaining { remaining: Amount, requested: Amount },
    DuplicateRequest,
    DuplicateDifferentBody,
    LinkedChainTooLong,
    LinkedChainUnterminated,
    QuarantineDraining,
    ConfigInvalid,
    SharedBudgetGroupRequired,
    PartialResolutionNotAllowed,
    SharedBudgetGroupIncomplete,
    BalanceOverflow,
    RaftCommitFailed,
    Overloaded,
}

impl LedgerError {
    /// Stable label for metrics: the payload varies, the category does not.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::InvalidFlags => "InvalidFlags",
            Self::InvalidAmount => "InvalidAmount",
            Self::SameAccount => "SameAccount",
            Self::UnknownAccount(_) => "UnknownAccount",
            Self::LedgerMismatch => "LedgerMismatch",
            Self::MissingPendingRef => "MissingPendingRef",
            Self::UnexpectedPendingRef => "UnexpectedPendingRef",
            Self::SeqGap { .. } => "SeqGap",
            Self::AccountQuarantined(_) => "AccountQuarantined",
            Self::FailStop => "FailStop",
            Self::InsufficientBalance { .. } => "InsufficientBalance",
            Self::PendingRefNotFound(_) => "PendingRefNotFound",
            Self::PendingRefMismatch => "PendingRefMismatch",
            Self::PendingRefAlreadyResolved => "PendingRefAlreadyResolved",
            Self::SettleExceedsRemaining { .. } => "SettleExceedsRemaining",
            Self::DuplicateRequest => "DuplicateRequest",
            Self::DuplicateDifferentBody => "DuplicateDifferentBody",
            Self::LinkedChainTooLong => "LinkedChainTooLong",
            Self::LinkedChainUnterminated => "LinkedChainUnterminated",
            Self::QuarantineDraining => "QuarantineDraining",
            Self::ConfigInvalid => "ConfigInvalid",
            Self::SharedBudgetGroupRequired => "SharedBudgetGroupRequired",
            Self::PartialResolutionNotAllowed => "PartialResolutionNotAllowed",
            Self::SharedBudgetGroupIncomplete => "SharedBudgetGroupIncomplete",
            Self::BalanceOverflow => "BalanceOverflow",
            Self::RaftCommitFailed => "RaftCommitFailed",
            Self::Overloaded => "Overloaded",
        }
    }
}

impl core::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for LedgerError {}
