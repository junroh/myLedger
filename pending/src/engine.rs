use ledger_base::ports::{HoldData, PendingEffect};
use ledger_base::{Amount, BudgetGroup, FxHashMap, MapGauge, TxId};

use crate::block::{BlockAddr, BlockStore, LogTraffic, MemBlockStore, RecordLog};
use crate::index::{Candidates, HoldTable};

/// What the pending engine keeps: every hold it was told to create, and the budget groups those
/// holds belong to. It writes down committed decisions and judges nothing — which is why a write
/// that names a hold the store does not have is dropped rather than refused.
#[derive(Default)]
pub struct PendingEngine {
    /// Where each hold is.
    index: HoldTable,
    /// What each hold is. Append-only, so a hold whose remainder changed is written again and the
    /// index repointed rather than a block rewritten in place.
    records: RecordLog,
    budgets: FxHashMap<BudgetGroup, BudgetState>,
    /// Reused by every compaction, so flushing a block allocates nothing.
    survivors: Vec<(TxId, HoldData, BlockAddr)>,
    /// Lookups waiting on the store, with the rest of their candidate walk.
    fetches: FxHashMap<u64, Fetch>,
    overflowed: u64,
    /// Committed decisions applied, counted so an answer can say which of them it reflects. The
    /// sequencer knows how many it had sent when it asked, and an answer from before one of them is a
    /// component that has reordered its own queue — which is the check that replaces the lane's order
    /// for a request that keeps no place in it.
    applied: u64,
}

/// Where a lookup has got to: which key it is for, the addresses its fingerprint matched, and how many
/// of them have been ruled out.
struct Fetch {
    key: TxId,
    candidates: Candidates,
    next: usize,
}

/// What starting a lookup did. `Busy` is the store refusing another read, which is backpressure — the
/// caller keeps the command and asks again.
#[derive(Debug, Clone, Copy)]
pub enum Started {
    Answered(Option<HoldData>),
    Fetching,
    Busy,
}

/// The group as the store knows it. Membership is why the sequencer can tell a partial
/// resolution from a complete one.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetState {
    members: u32,
    remaining: Amount,
}

impl PendingEngine {
    /// Sized from what the configuration declared. The table does not grow, so this is the only place its
    /// size is decided.
    pub fn sized(
        slots: usize,
        flush_blocks: usize,
        resident_blocks: usize,
        store: Box<dyn BlockStore>,
    ) -> Self {
        Self {
            index: HoldTable::with_slots(slots),
            records: RecordLog::new(store, flush_blocks, resident_blocks),
            ..Self::default()
        }
    }

    /// Windows of its own, for a test that wants the buffer small enough to see it compact, or
    /// residency small enough to see a record leave memory.
    pub fn with_windows(flush_blocks: usize, resident_blocks: usize) -> Self {
        Self {
            records: RecordLog::new(
                Box::new(MemBlockStore::default()),
                flush_blocks,
                resident_blocks,
            ),
            ..Self::default()
        }
    }

    /// Answers with the hold and, when it belongs to one, its group as a whole. The group rides
    /// along because a resolution cannot be checked for coverage without both.
    ///
    /// Takes `&mut self` because reading a record may reach past the block being filled, and the
    /// buffer it is read into belongs to the engine — a lookup that allocated one would allocate per
    /// request.
    pub fn lookup(&mut self, pending_ref: TxId) -> Option<HoldData> {
        let hold = self.read(pending_ref)?;
        Some(self.with_group(hold))
    }

    /// Starts a lookup without waiting for the store. What the index gives is candidates — addresses
    /// whose fingerprint matches — and only a record settles which is really this key, so the walk stops
    /// at the first candidate that is in memory and matches, and otherwise asks the store for one and
    /// carries the rest of the walk with it. Answering "not there" on a fingerprint collision instead
    /// would reject a hold that exists, a few times in every ten thousand.
    pub fn begin_lookup(&mut self, handle: u64, key: TxId, now: u64) -> Started {
        let candidates = self.index.candidates(key);
        self.walk(
            Fetch {
                key,
                candidates,
                next: 0,
            },
            handle,
            now,
        )
    }

    /// The next lookup the store has finished, if its record turns out to be the key that was asked
    /// for. When it does not, the walk continues and this answers nothing yet.
    pub fn harvest(&mut self, now: u64) -> Option<(u64, Option<HoldData>)> {
        while let Some((handle, _, found, hold)) = self.records.harvest(now) {
            let Some(fetch) = self.fetches.remove(&handle) else {
                continue;
            };
            if found == fetch.key {
                return Some((handle, Some(self.with_group(hold))));
            }
            // A different hold shares the fingerprint. Keep walking; the answer is not in yet.
            match self.walk(fetch, handle, now) {
                Started::Answered(answer) => return Some((handle, answer)),
                Started::Fetching | Started::Busy => continue,
            }
        }
        None
    }

    /// Fetches asked of the store and not yet answered.
    pub fn inflight(&self) -> usize {
        self.fetches.len()
    }

    fn walk(&mut self, mut fetch: Fetch, handle: u64, now: u64) -> Started {
        while fetch.next < fetch.candidates.len() {
            let addr = fetch.candidates.address(fetch.next);
            fetch.next += 1;
            match self.records.try_read(addr) {
                Some((found, hold)) if found == fetch.key => {
                    return Started::Answered(Some(self.with_group(hold)))
                }
                // In memory and somebody else's: the fingerprint matched and the key did not. With a
                // whole hash for a fingerprint this is beyond unreachable in practice — it cannot be
                // constructed by search, which is why there is no test for it — but "beyond unreachable"
                // is a probability and not a proof, and answering absent here would reject a hold that
                // exists. It was reachable, and tested, while the fingerprint was sixteen bits.
                Some(_) => continue,
                None => {
                    if !self.records.fetch(handle, addr, now) {
                        return Started::Busy;
                    }
                    self.fetches.insert(handle, fetch);
                    return Started::Fetching;
                }
            }
        }
        Started::Answered(None)
    }

    fn with_group(&self, mut hold: HoldData) -> HoldData {
        if let Some(state) = self.budgets.get(&hold.budget) {
            hold.budget_members = state.members;
            hold.budget_remaining = state.remaining;
        }
        hold
    }

    /// Whether the record at this address carries this key. Only asked when the index says a fingerprint
    /// is shared, which in a deployment that has never seen a collision is never.
    fn verifier(records: &mut RecordLog, key: TxId) -> impl FnMut(BlockAddr) -> bool + '_ {
        move |addr| records.read(addr).is_some_and(|(found, _)| found == key)
    }

    /// Committed decisions applied so far. An answer carries this, so the sequencer can tell an answer
    /// taken after the writes it had sent from one taken before them.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    pub fn write(&mut self, effect: PendingEffect) {
        self.applied += 1;
        match effect {
            PendingEffect::Create {
                tx_id,
                debit_account,
                credit_account,
                amount,
                ledger,
                budget,
            } => {
                self.put(
                    tx_id,
                    HoldData {
                        debit_account,
                        credit_account,
                        amount,
                        remaining: amount,
                        ledger,
                        budget,
                        budget_members: 0,
                        budget_remaining: 0,
                    },
                    true,
                );
                if !budget.is_absent() {
                    let state = self.budgets.entry(budget).or_default();
                    state.members += 1;
                    state.remaining += amount;
                }
            }
            // Nothing is read when the overlay still had the record: append-only needs the whole record
            // to write it again, and the decision brought it. Without it — a resolution judged inside the
            // chain that created the hold, whose overlay entry the sequencer never had — the old version
            // is read back, which is what this used to cost every time.
            PendingEffect::Reduce {
                pending_ref,
                debit_account,
                credit_account,
                amount,
                remaining,
                consumed,
                ledger,
                budget,
            } => {
                // Everything the record needs came with the decision, unless the sequencer could not
                // supply the hold's original size — then the old version is read for it.
                let amount = if amount > 0 {
                    amount
                } else {
                    match self.read(pending_ref) {
                        Some(old) => old.amount,
                        None => return,
                    }
                };
                let hold = HoldData {
                    debit_account,
                    credit_account,
                    amount,
                    remaining,
                    ledger,
                    budget,
                    budget_members: 0,
                    budget_remaining: 0,
                };
                self.put(pending_ref, hold, false);
                if let Some(state) = self.budgets.get_mut(&budget) {
                    state.remaining -= consumed;
                }
            }
            // Nothing is read: the slot is found by address and the group's arithmetic came with the
            // decision. This is every full settle and every void, which is most of the traffic.
            PendingEffect::Remove {
                pending_ref,
                budget,
                released,
            } => {
                let Self { index, records, .. } = self;
                let mut verify = Self::verifier(records, pending_ref);
                if index.remove(pending_ref, &mut verify).is_none() {
                    return;
                }
                if budget.is_absent() {
                    return;
                }
                let Some(state) = self.budgets.get_mut(&budget) else {
                    return;
                };
                state.members = state.members.saturating_sub(1);
                state.remaining -= released;
                if state.members == 0 {
                    self.budgets.remove(&budget);
                }
            }
        }
    }

    /// The maps are the engine's own, so their size is its to report; the gauges are where another
    /// thread may read it. The index's capacity is its slots rather than its entries: what a hold
    /// costs in the index is a slot whether or not one is occupied.
    pub(crate) fn publish(
        &self,
        holds: &MapGauge,
        budgets: &MapGauge,
        blocks: &MapGauge,
        buffer: &MapGauge,
        resident: &MapGauge,
    ) {
        holds.publish(self.index.len(), self.index.slots());
        budgets.publish(self.budgets.len(), self.budgets.capacity());
        let (buffered, in_memory, stored) = self.records.blocks();
        let (flush_window, resident_window) = self.records.windows();
        // A store block is written once and freed only when its segment expires, so the count is its
        // own ceiling; the two memory zones each have the window they were given.
        blocks.publish(stored, stored);
        buffer.publish(buffered, flush_window);
        resident.publish(in_memory, resident_window);
    }

    pub fn traffic(&self) -> LogTraffic {
        let (_, worst_cascade) = self.index.kick_stats();
        LogTraffic {
            index_live: self.index.len(),
            index_slots: self.index.slots(),
            worst_cascade,
            ambiguous: self.index.ambiguous(),
            overflowed: self.overflowed,
            ..self.records.traffic()
        }
    }

    /// Carries the oldest buffered block's survivors on and drops the rest. A record is alive exactly
    /// when the index still points at it, which needs no extra bookkeeping: a resolved hold has no
    /// entry and a superseded version's entry has moved on. Both tests are address comparisons, so
    /// compaction reads nothing it has not already got in hand.
    fn compact(&mut self) {
        while self.records.over_window() {
            self.survivors.clear();
            let mut died = 0;
            for (key, hold, addr) in self.records.oldest_block() {
                if self.index.points_at(key, addr) {
                    self.survivors.push((key, hold, addr));
                } else {
                    died += 1;
                }
            }
            for index in 0..self.survivors.len() {
                let (key, hold, old) = self.survivors[index];
                let new = self.records.keep(key, &hold);
                self.index.replace(key, old, new);
            }
            self.records.drop_oldest(died);
        }
    }

    /// Appends the record and points the key at it. Both halves are one call because a record the
    /// index does not point at is unreachable and an index entry with no record is a wrong answer.
    ///
    /// `fresh` is what lets the index check uniqueness: a hold the store has never held is the one moment
    /// a shared fingerprint can be noticed for free.
    fn put(&mut self, key: TxId, hold: HoldData, fresh: bool) {
        let addr = self.records.append(key, &hold);
        let Self { index, records, .. } = self;
        if fresh {
            if index.insert_new(key, addr).is_err() {
                // The table was sized for a maximum this has passed. Nothing here can fix it, and the
                // hold is not written down — which is why the count is reported rather than swallowed.
                self.overflowed += 1;
                return;
            }
        } else {
            let mut verify = Self::verifier(records, key);
            index.repoint(key, addr, &mut verify);
        }
        self.compact();
    }

    fn read(&mut self, key: TxId) -> Option<HoldData> {
        let addr = {
            let Self { index, records, .. } = self;
            let mut verify = Self::verifier(records, key);
            index.addr_of(key, &mut verify)?
        };
        self.records.read(addr).map(|(_, hold)| hold)
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount, budget: BudgetGroup) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget,
        }
    }

    /// The decision blocks are written once buys: a partial resolution appends a new version and the
    /// index follows it, instead of a block being read, changed and written back. What the engine
    /// answers has to be the new remainder, and the old record has to still be sitting there — that
    /// is the cost, and a version silently overwritten in place would hide it.
    #[test]
    fn a_partial_resolution_appends_a_version_rather_than_rewriting_one() {
        let mut engine = PendingEngine::default();
        engine.write(create(TxId(9), 100, BudgetGroup::ABSENT));
        let (_, after_create) = (engine.records.blocks(), engine.records.appended());

        engine.write(PendingEffect::Reduce {
            pending_ref: TxId(9),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 100,
            remaining: 40,
            consumed: 60,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        assert_eq!(
            engine.records.appended(),
            after_create + 1,
            "the reduction did not append a record"
        );
        assert_eq!(engine.lookup(TxId(9)).map(|hold| hold.remaining), Some(40));

        engine.write(PendingEffect::Reduce {
            pending_ref: TxId(9),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 100,
            remaining: 10,
            consumed: 30,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        assert_eq!(engine.records.appended(), after_create + 2);
        assert_eq!(engine.lookup(TxId(9)).map(|hold| hold.remaining), Some(10));

        engine.write(PendingEffect::Remove {
            pending_ref: TxId(9),
            budget: BudgetGroup::ABSENT,
            released: 10,
        });
        assert!(
            engine.lookup(TxId(9)).is_none(),
            "a removed hold is gone from the index"
        );
        assert_eq!(
            engine.records.appended(),
            after_create + 2,
            "a removal writes nothing; the records wait for their segment to expire"
        );
    }

    /// A group's remainder follows the writes the engine is told about, across the block boundary a
    /// partial resolution pushes it over. Holds are found by index, so nothing depends on a record
    /// staying in the block it was first written to.
    #[test]
    fn a_group_survives_its_records_spilling_into_later_blocks() {
        let group = BudgetGroup(77);
        let mut engine = PendingEngine::default();
        let members = RECORDS_PER_BLOCK + 3;
        for member in 0..members {
            engine.write(create(TxId(member as u128 + 1), 10, group));
        }
        let hold = engine.lookup(TxId(1)).expect("the first member");
        assert_eq!(hold.budget_members, members as u32);
        assert_eq!(hold.budget_remaining, members as Amount * 10);

        engine.write(PendingEffect::Reduce {
            pending_ref: TxId(1),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 10,
            remaining: 4,
            consumed: 6,
            ledger: 1,
            budget: group,
        });
        let last = engine
            .lookup(TxId(members as u128))
            .expect("the last member");
        assert_eq!(last.budget_remaining, members as Amount * 10 - 6);
        assert_eq!(last.budget_members, members as u32);
    }
}

#[cfg(test)]
mod apply_tests {
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    /// The property a sixteen-byte slot buys: applying a committed decision reads **nothing**, however
    /// cold the hold is. A whole hash for a fingerprint makes a match identity, so no slot has to be
    /// confirmed against a record — and the decision carries the record a partial resolution needs to
    /// append. Apply is in order, so a read here would be an IO nothing can hide.
    #[test]
    fn applying_a_decision_never_reads_the_store() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let cold = TxId(1);
        engine.write(create(cold, 500));
        let hold_amount = engine.lookup(cold).expect("created").amount;

        // Push it out of the buffer and into the store, then forget what that cost.
        for index in 0..RECORDS_PER_BLOCK * 3 {
            engine.write(create(TxId(1_000 + index as u128), 10));
        }
        let before = engine.traffic().apply_store_reads;

        engine.write(PendingEffect::Reduce {
            pending_ref: cold,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: hold_amount,
            remaining: 300,
            consumed: 200,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        engine.write(PendingEffect::Remove {
            pending_ref: cold,
            budget: BudgetGroup::ABSENT,
            released: 300,
        });
        assert_eq!(
            engine.traffic().apply_store_reads,
            before,
            "applying a decision about a cold hold read the store"
        );
        assert!(engine.lookup(cold).is_none(), "the hold was not resolved");
    }

    /// And when the decision carries no record — a resolution judged inside the chain that created the
    /// hold, which the sequencer's overlay never held — the engine reads it back. An optimisation with a
    /// fallback, not a fact anything rests on.
    #[test]
    fn a_decision_without_the_record_falls_back_to_reading_it() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let cold = TxId(2);
        engine.write(create(cold, 500));
        for index in 0..RECORDS_PER_BLOCK * 3 {
            engine.write(create(TxId(2_000 + index as u128), 10));
        }
        let before = engine.traffic().apply_store_reads;

        // No original size supplied, which is what sends the engine to the record it already has.
        engine.write(PendingEffect::Reduce {
            pending_ref: cold,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 0,
            remaining: 100,
            consumed: 400,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        assert!(
            engine.traffic().apply_store_reads > before,
            "the fallback did not read the record it needed"
        );
        assert_eq!(engine.lookup(cold).map(|hold| hold.remaining), Some(100));
    }

    /// The two windows are independent, and this is the property that says so: a record written to the
    /// store is still answered from memory, and it stops being answered from memory only when residency
    /// ends — not when it was written. Collapsing the two would make every flushed record an IO, which is
    /// the whole cost this exists to avoid.
    #[test]
    fn a_written_record_is_still_answered_from_memory_until_residency_ends() {
        // One block unwritten, two blocks written and kept. A survivor of the flush window therefore
        // lands in the store and stays readable, until two blocks' worth of later survivors push it out.
        let mut engine = PendingEngine::with_windows(1, 2);
        let survivor = TxId(7);
        engine.write(create(survivor, 500));
        // Enough later holds to compact the survivor's block out of the buffer, but not enough to fill
        // residency: three blocks of survivors would, so two is the most that can be asked for here.
        for index in 0..RECORDS_PER_BLOCK * 2 {
            engine.write(create(TxId(1_000 + index as u128), 10));
        }

        let carried = engine.traffic().flushed;
        assert!(
            carried > 0,
            "nothing was carried on, so the flush window never closed"
        );
        let before = engine.traffic();
        assert_eq!(engine.lookup(survivor).map(|hold| hold.amount), Some(500));
        let after = engine.traffic();
        assert_eq!(
            after.store_reads, before.store_reads,
            "a resident record cost an IO"
        );
        assert!(
            after.resident_reads > before.resident_reads,
            "it was not read from residency"
        );

        // Now push it past residency. Its content is on the store, so nothing is lost — but answering
        // for it is an IO from here on, which is exactly what `left_memory` counts.
        for index in 0..RECORDS_PER_BLOCK * 6 {
            engine.write(create(TxId(9_000 + index as u128), 10));
        }
        assert!(
            engine.traffic().left_memory > 0,
            "residency never dropped a block"
        );
        let before = engine.traffic();
        assert_eq!(engine.lookup(survivor).map(|hold| hold.amount), Some(500));
        assert!(
            engine.traffic().store_reads > before.store_reads,
            "a record past residency was still answered from memory"
        );
    }
}

#[cfg(test)]
mod buffer_tests {
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    fn resolve(tx_id: TxId, released: Amount) -> PendingEffect {
        PendingEffect::Remove {
            pending_ref: tx_id,
            budget: BudgetGroup::ABSENT,
            released,
        }
    }

    /// The saving the whole capacity estimate rests on: a hold resolved before its block is compacted
    /// never reaches the store. Without this the store would grow with holds created rather than holds
    /// alive, and the design's own figure for how much disk it needs would be the wrong one.
    #[test]
    fn a_hold_resolved_before_its_block_is_compacted_never_reaches_the_store() {
        // One block of window, so the second block being filled compacts the first.
        let mut engine = PendingEngine::with_windows(1, 1);
        let holds = RECORDS_PER_BLOCK;
        for index in 0..holds {
            engine.write(create(TxId(index as u128 + 1), 10));
        }
        // Every one of them resolved while still buffered.
        for index in 0..holds {
            let key = TxId(index as u128 + 1);
            engine.write(resolve(key, 10));
        }
        // One more record starts a second block, which is what puts the first one over the window.
        // Only that far: filling the second block too would compact live records and the count below
        // would be measuring those instead.
        engine.write(create(TxId(1_000_000), 10));
        let traffic = engine.traffic();
        assert!(
            traffic.died_in_buffer >= holds as u64,
            "records were carried on that were dead"
        );
        assert_eq!(traffic.flushed, 0, "a dead record was written to the store");
        assert_eq!(
            traffic.store_reads, 0,
            "nothing should have been read from the store yet"
        );
    }

    /// And a hold that outlives the window is carried on with its index entry following it, so it is
    /// still found afterwards. Compaction moves addresses; an entry left behind would be a hold the
    /// log says exists and the engine cannot find.
    #[test]
    fn a_survivor_is_carried_on_and_still_found_at_its_new_address() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let survivor = TxId(7);
        engine.write(create(survivor, 500));

        // Fill two blocks past it, so its block is compacted out.
        for index in 0..RECORDS_PER_BLOCK * 2 + 2 {
            engine.write(create(TxId(1_000 + index as u128), 10));
        }
        let after = engine
            .lookup(survivor)
            .expect("the survivor is still there");
        assert_eq!(after.remaining, 500, "the survivor came back changed");
        assert!(
            engine.traffic().flushed > 0,
            "nothing was carried on at all"
        );

        // And it is still the hold the index says it is, at whatever address compaction gave it.
        engine.write(resolve(survivor, 500));
        assert!(
            engine.lookup(survivor).is_none(),
            "the survivor was not resolved"
        );
    }

    /// A partial resolution's old version is dead the moment the index moves on, so compaction drops it
    /// without being told. That is what makes append-only affordable: the garbage is identified by the
    /// index rather than tracked.
    #[test]
    fn a_superseded_version_is_dropped_without_being_tracked() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let hold = TxId(11);
        engine.write(create(hold, 100));
        let hold_amount = engine.lookup(hold).expect("created").amount;
        engine.write(PendingEffect::Reduce {
            pending_ref: hold,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: hold_amount,
            remaining: 60,
            consumed: 40,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });

        for index in 0..RECORDS_PER_BLOCK * 2 + 2 {
            engine.write(create(TxId(2_000 + index as u128), 10));
        }
        assert_eq!(engine.lookup(hold).map(|found| found.remaining), Some(60));
        let traffic = engine.traffic();
        assert!(
            traffic.died_in_buffer > 0,
            "the superseded version was carried on as if it were alive"
        );
    }
}

#[cfg(test)]
mod fetch_tests {
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    /// Pushes enough records through that everything written before is compacted into the store.
    fn cool(engine: &mut PendingEngine, from: u128) {
        for index in 0..RECORDS_PER_BLOCK * 3 {
            engine.write(create(TxId(from + index as u128), 10));
        }
    }

    fn answered(started: Started) -> Option<HoldData> {
        match started {
            Started::Answered(found) => found,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// A hold still in the buffer is answered where the command was taken off the queue: no fetch, so
    /// nothing to wait for. This is the common case and it must not pay for the cold one.
    #[test]
    fn a_buffered_hold_is_answered_without_a_fetch() {
        let mut engine = PendingEngine::with_windows(8, 8);
        engine.write(create(TxId(3), 100));
        let found = answered(engine.begin_lookup(1, TxId(3), 0));
        assert_eq!(found.map(|hold| hold.remaining), Some(100));
        assert_eq!(engine.inflight(), 0, "a buffered hold should need no fetch");
    }

    /// A hold the buffer no longer holds is fetched, and the answer arrives from the harvest rather
    /// than from the call. This is the path a device's latency lands on.
    #[test]
    fn a_cold_hold_is_fetched_and_answered_when_it_completes() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let cold = TxId(1);
        engine.write(create(cold, 250));
        cool(&mut engine, 1_000);

        match engine.begin_lookup(7, cold, 0) {
            Started::Fetching => {}
            other => panic!("a cold hold should be fetched, got {other:?}"),
        }
        assert_eq!(engine.inflight(), 1);

        let (handle, found) = engine.harvest(0).expect("the fetch completes");
        assert_eq!(handle, 7, "the answer came back under another handle");
        assert_eq!(found.map(|hold| hold.remaining), Some(250));
        assert_eq!(engine.inflight(), 0);
        assert!(engine.harvest(0).is_none(), "nothing else was outstanding");
    }

    /// A hold nobody ever created is answered as absent, and asking again gets the same answer without
    /// touching the store.
    #[test]
    fn a_hold_that_was_never_created_is_answered_absent() {
        let mut engine = PendingEngine::with_windows(1, 1);
        cool(&mut engine, 1);
        assert!(answered(engine.begin_lookup(1, TxId(999_999), 0)).is_none());
        assert_eq!(engine.inflight(), 0);
    }
}
