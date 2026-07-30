pub type Amount = i64;
pub type Seq = u64;

/// Per-transfer ceiling, low enough that cumulative `i64` totals keep headroom.
pub const MAX_AMOUNT: Amount = 1 << 40;

macro_rules! id_type {
    ($(#[$attr:meta])* $name:ident($inner:ty)) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[repr(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            pub const ABSENT: Self = Self(0);

            pub const fn is_absent(self) -> bool {
                self.0 == 0
            }

            pub const fn raw(self) -> $inner {
                self.0
            }
        }
    };
}

id_type!(TxId(u128));
id_type!(AccountId(u64));
id_type!(
    /// One linked chain: an atomicity unit that lives for a single judge and a single propose.
    LinkedChainId(u32)
);

id_type!(
    /// A shared budget group: a lifetime property of holds that must be resolved together,
    /// which outlives the request that created them. The client names it — conventionally
    /// after the first hold of the group — and declares it in a hold's `pending_ref`.
    BudgetGroup(u128)
);

/// Dense index into the account store, resolved once at intake so later stages
/// never re-hash an `AccountId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct AcctHandle(u32);

impl AcctHandle {
    pub const INVALID: Self = Self(u32::MAX);

    pub const fn new(index: usize) -> Self {
        Self(index as u32)
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl Default for AcctHandle {
    fn default() -> Self {
        Self::INVALID
    }
}
