use ledger_base::ports::{AccountFlags, AccountPort, AccountRecord, LedgerTotals};
use std::mem::size_of;

use ledger_base::{AccountId, AcctHandle, Effect, Footprint, FxHashMap, LedgerError};

/// In-memory tier of the account component. Every account is resident in DRAM because the
/// judge reads it inline; durability is this component's own concern (checkpoint plus log
/// replay), not something the sequencer waits for. The disk tier is not built yet.
pub struct MemoryAccounts {
    records: Vec<AccountRecord>,
    ids: Vec<AccountId>,
    index: FxHashMap<AccountId, AcctHandle>,
    applied: u64,
}

impl MemoryAccounts {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            records: Vec::with_capacity(capacity),
            ids: Vec::with_capacity(capacity),
            index: FxHashMap::with_capacity_and_hasher(capacity, Default::default()),
            applied: 0,
        }
    }

    pub fn open(&mut self, id: AccountId, ledger: u32, flags: AccountFlags) -> AcctHandle {
        if let Some(handle) = self.index.get(&id) {
            return *handle;
        }
        let handle = AcctHandle::new(self.records.len());
        self.records.push(AccountRecord::new(ledger, flags));
        self.ids.push(id);
        self.index.insert(id, handle);
        handle
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// What this component is holding. Accounts never leave, so there is no peak apart from the
    /// count: this is the part of a sizing answer that is arithmetic on the working set rather than a
    /// consequence of how the run went.
    pub fn footprint(&self) -> Footprint {
        let live = self.records.len();
        let mut footprint = Footprint::new();
        // No ceiling: an account store is as big as the accounts in it, so there is no bound for a
        // peak to be measured against.
        footprint.other(
            "account records",
            live,
            live,
            0,
            self.records.capacity() * size_of::<AccountRecord>()
                + self.ids.capacity() * size_of::<AccountId>(),
        );
        let mut index = Footprint::new();
        index.hash_table::<AccountId, AcctHandle>("account index", live, self.index.capacity(), live);
        for part in index.parts() {
            footprint.other(part.name, part.entries, part.peak_entries, 0, part.bytes);
        }
        footprint
    }

    fn column_totals(&self) -> LedgerTotals {
        let mut totals = LedgerTotals::default();
        for record in &self.records {
            totals.debits_posted += record.debits_posted();
            totals.credits_posted += record.credits_posted();
            totals.debits_pending += record.debits_pending();
            totals.credits_pending += record.credits_pending();
        }
        totals
    }
}

impl AccountPort for MemoryAccounts {
    fn resolve(&self, id: AccountId) -> Option<AcctHandle> {
        self.index.get(&id).copied()
    }

    /// Handles only come from `open` or `resolve`, so an out-of-range handle is a caller bug.
    fn record(&self, handle: AcctHandle) -> &AccountRecord {
        debug_assert!(handle.index() < self.records.len());
        &self.records[handle.index()]
    }

    fn apply(&mut self, effect: &Effect) -> Result<(), LedgerError> {
        self.move_columns(effect.debit, effect.credit, effect)
    }

    fn applied(&self) -> u64 {
        self.applied
    }

    fn totals(&self) -> LedgerTotals {
        self.column_totals()
    }
}

impl MemoryAccounts {
    /// Applies a committed effect by account id rather than by the leader's handle, which is
    /// what a follower or a recovering node does: handles are leader-local, ids are not.
    pub fn replay(&mut self, effect: &Effect) -> Result<(), LedgerError> {
        let debit = self
            .resolve(effect.debit_account)
            .ok_or(LedgerError::UnknownAccount(effect.debit_account))?;
        let credit = self
            .resolve(effect.credit_account)
            .ok_or(LedgerError::UnknownAccount(effect.credit_account))?;
        self.move_columns(debit, credit, effect)
    }

    /// Both sides move or neither does. A debit written without its credit would break the
    /// accounting identity for good — the effect is already committed, so there is no way back —
    /// which is why the far side is asked before the near side is touched. The two sides write
    /// different columns, so an effect whose debit and credit are the same account is no
    /// special case.
    fn move_columns(
        &mut self,
        debit: AcctHandle,
        credit: AcctHandle,
        effect: &Effect,
    ) -> Result<(), LedgerError> {
        let columns = effect.columns();
        if !self.records[credit.index()].fits_credit(columns) {
            return Err(LedgerError::BalanceOverflow);
        }
        self.records[debit.index()].apply_debit(columns)?;
        self.records[credit.index()].apply_credit(columns)?;
        self.applied += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::ports::{AccountFlags, AccountPort};
    use ledger_base::{AccountId, Amount, Effect, EffectKind, LedgerError, TxId, MAX_AMOUNT};

    use super::*;

    const LEDGER: u32 = 1;
    const FROM: AccountId = AccountId(1);
    const TO: AccountId = AccountId(2);

    fn post(store: &MemoryAccounts, amount: Amount) -> Effect {
        Effect {
            tx_id: TxId(1),
            pending_ref: TxId::ABSENT,
            debit_account: FROM,
            credit_account: TO,
            amount,
            remaining_after: 0,
            debit: store.resolve(FROM).expect("open"),
            credit: store.resolve(TO).expect("open"),
            chain: Default::default(),
            budget: Default::default(),
            ledger: LEDGER,
            kind: EffectKind::Post,
        }
    }

    /// An effect either moves both sides or neither. A debit written without its credit would break
    /// the accounting identity permanently, since the effect is already committed and there is no
    /// way back — so the overflow has to be refused before anything is written.
    #[test]
    fn an_effect_that_cannot_land_on_both_sides_lands_on_neither() {
        let mut store = MemoryAccounts::with_capacity(4);
        store.open(FROM, LEDGER, AccountFlags::NONE);
        store.open(TO, LEDGER, AccountFlags::CONSTRAINED);

        // Fill the credit side to just under the ceiling, one whole effect at a time.
        let step = MAX_AMOUNT;
        let mut landed = 0;
        while landed < Amount::MAX - step {
            if store.apply(&post(&store, step)).is_err() {
                break;
            }
            landed += step;
        }
        let before = store.totals();

        let refused = store.apply(&post(&store, step));

        assert_eq!(refused, Err(LedgerError::BalanceOverflow));
        assert_eq!(store.totals(), before, "a refused effect must leave both sides untouched");
        assert_eq!(
            store.totals().debits_posted,
            store.totals().credits_posted,
            "posted identity"
        );
    }
}
