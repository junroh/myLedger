//! What the engine has to write down so a log can be truncated: the index, and the group totals beside it.
//!
//! Design notes §15 is the reasoning. The short of it: the blocks are already on disk and immutable, so the
//! only thing a snapshot has to carry is which of the records on them are alive — and that lives in the
//! index and nowhere else, because a resolution appends nothing a scan could see.
//!
//! **The same bytes serve two readers.** A follower too far behind gets them instead of log entries, and a
//! node that restarts reads them instead of replaying from nothing. So this is a byte stream rather than a
//! file: what it is written to is the caller's business, and there is no trait for it yet because there is
//! one destination.
//!
//! **Chunked from the start**, because the whole is 42.7GB at the design's size and nothing may hold that
//! in one buffer or write it in one go. Every record in the format is thirty-two bytes wide, so a chunk
//! boundary can fall anywhere a multiple of thirty-two does and neither side has to carry a partial item.
//!
//! **Coverage** is the log position everything up to which has reached a block. It is in the header, and it
//! is not the position of the last effect applied: the writeback buffer holds records from batches after it,
//! whose slots this leaves out, so replay starts *after* coverage and creates them again. A snapshot that
//! claimed the later position would be claiming records it does not carry.
//!
//! **The read is stable** while the engine keeps writing, and the reason it has to be is the kick cascade
//! rather than the effects: an entry displaced between buckets mid-dump appears twice in the stream or
//! nowhere, and a relocation is in no log for a replay to repair. `HoldTable::begin_snapshot` copies a bucket
//! the snapshot has not reached before it changes, and drops the copy as it is read.
//!
//! **The group totals travel with the position they reflect**, which is this snapshot's own instant and not
//! its coverage. They are accumulated rather than derived, so a replay that did not know that would count
//! every member created between the two a second time — see `PendingEngine::replay`.
//!
//! What is **not** here is where a snapshot goes: nothing writes one anywhere, and the throttle that would
//! pace the write is a number nobody has chosen. `status.md` tracks both.

use ledger_base::ports::ApplyIndex;
use ledger_base::{Amount, BudgetGroup, FxHashMap};

use crate::block::RecordLog;
use crate::engine::BudgetState;
use crate::index::{address_in, HoldTable};

/// Every record in the format, header included. Four eight-byte slots make a bucket exactly this wide, and a
/// group entry fits inside it, so the stream is a sequence of same-sized records and a chunk never splits one.
pub const RECORD: usize = 32;

/// The header takes two, because six fields do not fit in one — and widening every record in the format to
/// hold them would waste the difference on every bucket, of which there are hundreds of millions. Wasting
/// half of one record once is the cheaper end of that trade.
const HEADER: u64 = 2;

/// Little-endian by declaration and not by inheritance, for the same reason the block format is (§12): the
/// moment these bytes leave the process they are a format, and a format that borrows the machine's byte
/// order is not one.
const MAGIC: u64 = 0x5041_5f53_4e41_5031;

/// Bumped when the layout of any record changes. A reader that does not know a version refuses the stream
/// rather than interpreting it, because the alternative is a table restored from bytes that meant something
/// else.
const VERSION: u32 = 3;

/// Why a stream could not be read. Every one of them is a refusal rather than a repair: a snapshot that
/// does not describe this table is not a snapshot to make the best of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotASnapshot {
    /// The first record was not a header this build wrote.
    Unrecognised,
    /// A version this build does not know.
    Version(u32),
    /// The table it describes is a different size from the one being restored into. A snapshot's buckets are
    /// positional — a slot holds a fingerprint rather than a key, so nothing can be placed again — which
    /// makes the bucket count part of what the bytes mean.
    Buckets { theirs: u64, ours: u64 },
    /// A chunk arrived that was not a whole number of records, or after the stream had ended.
    Malformed,
}

/// A snapshot being written, one chunk at a time.
///
/// It owns what it needs rather than borrowing the engine, and that is not a style choice: a paced dump spans
/// many worker rounds, and a borrow that lasted that long would forbid the engine to apply anything for the
/// whole of it. What keeps the read stable instead is the index's shadow — see `HoldTable::begin_snapshot`.
///
/// The group totals are copied at the start. That is a real cost at the design's scale, and it is the one
/// part of a snapshot that is not read incrementally: they change while the dump runs, and the position they
/// reflect is what makes them replay-safe, so they have to be one instant's worth.
pub struct SnapshotWriter {
    coverage: ApplyIndex,
    groups_reflect: ApplyIndex,
    groups: Vec<(BudgetGroup, BudgetState)>,
    buckets: u64,
    /// Records already written, header included, so a chunk resumes rather than restarting.
    at: u64,
}

impl SnapshotWriter {
    pub fn new(
        buckets: usize,
        budgets: &FxHashMap<BudgetGroup, BudgetState>,
        coverage: ApplyIndex,
        groups_reflect: ApplyIndex,
    ) -> Self {
        // Sorted, so two writers over the same state produce the same bytes. A map's iteration order is not a
        // promise, and a snapshot that differed between nodes for no reason would be one nothing could compare.
        let mut groups: Vec<(BudgetGroup, BudgetState)> =
            budgets.iter().map(|(id, state)| (*id, *state)).collect();
        groups.sort_by_key(|(id, _)| id.raw());
        Self {
            coverage,
            groups_reflect,
            groups,
            buckets: buckets as u64,
            at: 0,
        }
    }

    /// Records the whole stream holds: the header, one per bucket, one per group.
    pub fn records(&self) -> u64 {
        HEADER + self.buckets + self.groups.len() as u64
    }

    pub fn bytes(&self) -> u64 {
        self.records() * RECORD as u64
    }

    pub fn is_complete(&self) -> bool {
        self.at == self.records()
    }

    /// Fills `into` with as many whole records as fit and answers how many bytes were written. Zero means the
    /// stream is finished, or that `into` was too small to hold one record.
    ///
    /// `index` is asked for each bucket in turn, and hands back the copy taken before it changed where one was
    /// needed. Sealed-block state is asked of `records`, because a slot pointing anywhere else is written out
    /// empty.
    pub fn next_chunk(
        &mut self,
        into: &mut [u8],
        index: &mut HoldTable,
        records: &RecordLog,
    ) -> usize {
        let mut written = 0;
        while written + RECORD <= into.len() {
            let Some(()) = self.write_one(&mut into[written..written + RECORD], index, records)
            else {
                break;
            };
            written += RECORD;
            self.at += 1;
        }
        written
    }

    fn write_one(&self, into: &mut [u8], index: &mut HoldTable, records: &RecordLog) -> Option<()> {
        match self.at {
            0 => {
                into.fill(0);
                into[0..8].copy_from_slice(&MAGIC.to_le_bytes());
                into[8..12].copy_from_slice(&VERSION.to_le_bytes());
                into[12..20].copy_from_slice(&self.buckets.to_le_bytes());
                into[20..24].copy_from_slice(&(self.groups.len() as u32).to_le_bytes());
                into[24..32].copy_from_slice(&self.coverage.raw().to_le_bytes());
                Some(())
            }
            1 => {
                into.fill(0);
                // The position the group totals reflect, which is this snapshot's own instant rather than the
                // coverage above — see `PendingEngine::replay`. The two differ by whatever the writeback
                // buffer is holding, and replay needs both.
                into[0..8].copy_from_slice(&self.groups_reflect.raw().to_le_bytes());
                Some(())
            }
            at if at < HEADER + self.buckets => {
                let words = index.bucket_for_snapshot((at - HEADER) as usize)?;
                for (way, word) in words.iter().enumerate() {
                    // A slot pointing at a record that is not on a sealed block is written out empty. Its
                    // record is in the writeback buffer or in the block still being filled, so it will not be
                    // there on restore, and an index entry naming a block nobody has is worse than a hold the
                    // log can create again.
                    let keep = *word != 0 && records.is_sealed(address_in(*word));
                    let out = if keep { *word } else { 0 };
                    into[way * 8..way * 8 + 8].copy_from_slice(&out.to_le_bytes());
                }
                Some(())
            }
            at => {
                let (id, state) = self.groups.get((at - self.buckets - HEADER) as usize)?;
                into.fill(0);
                into[0..16].copy_from_slice(&id.raw().to_le_bytes());
                into[16..24].copy_from_slice(&state.remaining().to_le_bytes());
                into[24..28].copy_from_slice(&state.members().to_le_bytes());
                Some(())
            }
        }
    }
}

/// Reads a stream back into a table and a group map. Takes chunks in the order they were written, because
/// a bucket's position in the stream *is* its position in the table — there are no keys to place it by.
pub struct SnapshotReader {
    buckets: u64,
    groups_expected: u64,
    coverage: ApplyIndex,
    groups_reflect: ApplyIndex,
    /// Records taken so far, header included.
    at: u64,
    groups: FxHashMap<BudgetGroup, BudgetState>,
}

impl SnapshotReader {
    pub fn new() -> Self {
        Self {
            buckets: 0,
            groups_expected: 0,
            coverage: ApplyIndex::default(),
            groups_reflect: ApplyIndex::default(),
            at: 0,
            groups: FxHashMap::default(),
        }
    }

    /// Takes one chunk. The index is written into as it goes rather than buffered, because buffering it
    /// would mean holding the whole thing twice.
    pub fn take_chunk(&mut self, bytes: &[u8], index: &mut HoldTable) -> Result<(), NotASnapshot> {
        if !bytes.len().is_multiple_of(RECORD) {
            return Err(NotASnapshot::Malformed);
        }
        for record in bytes.chunks_exact(RECORD) {
            self.take_one(record, index)?;
            self.at += 1;
        }
        Ok(())
    }

    fn take_one(&mut self, record: &[u8], index: &mut HoldTable) -> Result<(), NotASnapshot> {
        let u64_at =
            |at: usize| u64::from_le_bytes(record[at..at + 8].try_into().expect("8 bytes"));
        let u32_at =
            |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().expect("4 bytes"));
        match self.at {
            0 => {
                if u64_at(0) != MAGIC {
                    return Err(NotASnapshot::Unrecognised);
                }
                let version = u32_at(8);
                if version != VERSION {
                    return Err(NotASnapshot::Version(version));
                }
                self.buckets = u64_at(12);
                self.groups_expected = u32_at(20) as u64;
                self.coverage = ApplyIndex(u64_at(24));
                let ours = index.bucket_count() as u64;
                if self.buckets != ours {
                    return Err(NotASnapshot::Buckets {
                        theirs: self.buckets,
                        ours,
                    });
                }
                Ok(())
            }
            1 => {
                self.groups_reflect = ApplyIndex(u64_at(0));
                Ok(())
            }
            at if at < HEADER + self.buckets => {
                let words = [u64_at(0), u64_at(8), u64_at(16), u64_at(24)];
                match index.restore_bucket((at - HEADER) as usize, words) {
                    true => Ok(()),
                    false => Err(NotASnapshot::Malformed),
                }
            }
            at if at < HEADER + self.buckets + self.groups_expected => {
                let id = BudgetGroup(u128::from_le_bytes(
                    record[0..16].try_into().expect("16 bytes"),
                ));
                let remaining = u64_at(16) as Amount;
                let members = u32_at(24);
                self.groups
                    .insert(id, BudgetState::restored(members, remaining));
                Ok(())
            }
            // Past the end: the header said how much there was, so anything after it is a stream that does
            // not agree with its own header.
            _ => Err(NotASnapshot::Malformed),
        }
    }

    /// Whether every record the header promised has arrived. A stream cut short leaves a table that is
    /// partly this snapshot and partly whatever it was, which is why the caller has to ask.
    pub fn is_complete(&self) -> bool {
        self.at == HEADER + self.buckets + self.groups_expected
    }

    /// The log position this snapshot's state reflects: replay starts after it. Meaningful once the header
    /// has arrived, which is the first record.
    pub fn coverage(&self) -> ApplyIndex {
        self.coverage
    }

    /// The position the group totals reflect — see `PendingEngine::groups_reflect`. Replay needs it beside
    /// the coverage above, because the two are different instants.
    pub fn groups_reflect(&self) -> ApplyIndex {
        self.groups_reflect
    }

    /// The group totals, once the stream is complete.
    pub fn into_groups(self) -> FxHashMap<BudgetGroup, BudgetState> {
        self.groups
    }
}

impl Default for SnapshotReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::ports::{ApplyIndex, PendingEffect};
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::block::{Block, DurableStore, MemoryStore, StoreFault, RECORDS_PER_BLOCK};
    use crate::engine::PendingEngine;

    /// A chunk small enough that every test here crosses several, because a format that is only ever read in
    /// one piece is not the format the engine will use.
    const CHUNK: usize = RECORD * 3;

    /// One block store two engines share, which is what a restore actually has: the snapshot carries the
    /// index and the blocks are already on the disk that survived. An engine restored over an empty store
    /// would find every slot and be able to read none of them, which is a test about nothing.
    #[derive(Clone, Default)]
    struct SharedStore(Rc<RefCell<MemoryStore>>);

    impl DurableStore for SharedStore {
        fn open_with(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
            self.0.borrow_mut().open_with(segment, offset, block)
        }

        fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
            self.0.borrow_mut().append(segment, offset, block)
        }

        fn read_at(
            &mut self,
            segment: u8,
            offset: u64,
            into: &mut Block,
        ) -> Result<(), StoreFault> {
            self.0.borrow_mut().read_at(segment, offset, into)
        }

        fn submit(&mut self, handle: u64, segment: u8, offset: u64, now: u64) -> bool {
            self.0.borrow_mut().submit(handle, segment, offset, now)
        }

        fn poll(&mut self, now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
            self.0.borrow_mut().poll(now, into)
        }

        fn inflight(&self) -> usize {
            self.0.borrow().inflight()
        }

        fn remove(&mut self, segment: u8) -> Result<(), StoreFault> {
            self.0.borrow_mut().remove(segment)
        }
    }

    /// A pair of engines over one store, sized alike so a snapshot of the first restores into the second.
    fn pair(slots: usize, flush_blocks: usize) -> (PendingEngine, PendingEngine) {
        let store = SharedStore::default();
        (
            PendingEngine::sized(slots, flush_blocks, 1024, Box::new(store.clone())),
            PendingEngine::sized(slots, flush_blocks, 1024, Box::new(store)),
        )
    }

    fn create(id: u128, budget: BudgetGroup) -> PendingEffect {
        PendingEffect::Create {
            tx_id: TxId(id),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 100,
            ledger: 1,
            budget,
        }
    }

    /// Writes the whole stream out chunk by chunk, then reads it back into a fresh engine.
    fn round_trip(from: &mut PendingEngine, into: &mut PendingEngine) -> Result<(), NotASnapshot> {
        let mut writer = from.begin_snapshot();
        let mut reader = SnapshotReader::new();
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let written = from.next_snapshot_chunk(&mut writer, &mut chunk);
            if written == 0 {
                break;
            }
            reader.take_chunk(&chunk[..written], into.index_mut())?;
        }
        assert!(
            reader.is_complete(),
            "the stream ended before its header said"
        );
        let coverage = reader.coverage();
        into.restore(reader.into_groups(), coverage);
        Ok(())
    }

    /// Every hold the snapshot carried answers the same after a restore.
    ///
    /// The holds are written until they leave the writeback buffer, because a record still in the buffer is
    /// one the snapshot deliberately does not carry — see the next test.
    #[test]
    fn a_restored_engine_answers_every_carried_hold_the_same() {
        let group = BudgetGroup(7);
        let (mut engine, mut restored) = pair(1 << 12, 1);
        let holds = RECORDS_PER_BLOCK * 3;
        for id in 1..=holds {
            engine
                .write(create(id as u128, group), ApplyIndex(id as u64))
                .expect("the index took the hold");
        }

        round_trip(&mut engine, &mut restored).expect("a snapshot of this engine");

        let mut carried = 0;
        for id in 1..=holds {
            let before = engine.lookup(TxId(id as u128));
            let after = restored.lookup(TxId(id as u128));
            match before {
                // Only the ones on sealed blocks are carried, and for those the two answers have to agree
                // field for field — the group's totals included, since those ride on the record.
                Some(hold) if after.is_some() => {
                    let after = after.expect("just matched");
                    assert_eq!(hold.remaining, after.remaining);
                    assert_eq!(hold.debit_account, after.debit_account);
                    assert_eq!(hold.credit_account, after.credit_account);
                    assert_eq!(hold.budget_members, after.budget_members);
                    assert_eq!(hold.budget_remaining, after.budget_remaining);
                    carried += 1;
                }
                _ => {}
            }
        }
        assert!(
            carried > 0,
            "the snapshot carried nothing, so the comparison proved nothing"
        );
        assert!(
            restored.counts_agree(),
            "a restored table's per-segment counts do not add up to its entries"
        );
    }

    /// A hold whose record has not reached a block is not carried, and that is the point rather than a gap.
    ///
    /// Its record lives in the writeback buffer, which the snapshot leaves out because the log has it — so
    /// carrying the index entry would name a block the restored engine does not have. Replay is what creates
    /// it again, and replay is not built, so here it is simply absent.
    #[test]
    fn a_hold_still_in_the_buffer_is_not_carried() {
        let (mut engine, mut restored) = pair(1 << 12, 64);
        engine
            .write(create(1, BudgetGroup::ABSENT), ApplyIndex(1))
            .expect("the index took the hold");
        assert!(
            engine.lookup(TxId(1)).is_some(),
            "the hold is not there to begin with"
        );

        round_trip(&mut engine, &mut restored).expect("a snapshot of this engine");

        assert!(
            restored.lookup(TxId(1)).is_none(),
            "an unflushed hold was carried, so the index names a block nobody has"
        );
        assert!(restored.counts_agree());
    }

    /// Coverage stops short of what has been applied by exactly what the writeback buffer is holding, and
    /// it advances as the buffer flushes.
    ///
    /// This is the claim the whole boundary rests on, and it is checkable without replay: everything up to
    /// coverage has reached a block, so a snapshot that carries only sealed slots carries everything that
    /// batch wrote. Claiming the later position — the last effect applied — would claim records still in the
    /// buffer, whose slots this deliberately leaves out.
    #[test]
    fn coverage_stops_where_the_buffer_begins_and_moves_as_it_flushes() {
        // A window of four blocks, so the buffer holds several and coverage lags visibly.
        let (mut engine, _) = pair(1 << 12, 4);
        assert_eq!(
            engine.coverage(),
            ApplyIndex(0),
            "an engine that has applied nothing claimed to cover something"
        );

        // One block's worth, all in the buffer: nothing is sealed, so nothing can be covered.
        for id in 1..=RECORDS_PER_BLOCK {
            engine
                .write(
                    create(id as u128, BudgetGroup::ABSENT),
                    ApplyIndex(id as u64),
                )
                .expect("the index took the hold");
        }
        assert_eq!(
            engine.coverage(),
            ApplyIndex(0),
            "coverage moved past a batch whose records are still in the buffer"
        );

        // Past the window, so the oldest blocks are compacted out and coverage follows them.
        let holds = RECORDS_PER_BLOCK * 8;
        for id in (RECORDS_PER_BLOCK + 1)..=holds {
            engine
                .write(
                    create(id as u128, BudgetGroup::ABSENT),
                    ApplyIndex(id as u64),
                )
                .expect("the index took the hold");
        }
        let covered = engine.coverage();
        assert!(
            covered.raw() > 0,
            "the buffer flushed and coverage stayed at nothing"
        );
        assert!(
            covered.raw() < holds as u64,
            "coverage reached the last batch applied, which is still in the buffer"
        );

        // And the claim itself: every hold at or below coverage is carried, every one above it is not.
        let mut restored = PendingEngine::sized(1 << 12, 4, 1024, Box::new(MemoryStore::default()));
        let mut writer = engine.begin_snapshot();
        let mut reader = SnapshotReader::new();
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let written = engine.next_snapshot_chunk(&mut writer, &mut chunk);
            if written == 0 {
                break;
            }
            reader
                .take_chunk(&chunk[..written], restored.index_mut())
                .expect("a stream this table can take");
        }
        assert_eq!(reader.coverage(), covered, "the header lost the position");
        for id in (covered.raw() + 1)..=holds as u64 {
            assert!(
                restored.lookup(TxId(id as u128)).is_none(),
                "a hold from after coverage was carried, so replay would create it twice"
            );
        }
    }

    /// Restore plus replay reproduces the engine, which is the whole chain and the reason for all of it.
    ///
    /// The log here is the test's own list of what was applied and where, because the real one is Raft's and
    /// Raft is a stand-in. What is being proved is the engine's half: that a snapshot covering an earlier
    /// position, plus every effect after it, lands on the same answers as never having stopped.
    ///
    /// Deliberately not asserted: that the two engines' blocks match. A replayed `Reduce` appends a version
    /// again, so the restored engine's layout differs — the index points at the newest either way, so every
    /// answer agrees and one record is wasted. Answers are the contract; layout is not.
    #[test]
    fn restore_and_replay_reproduce_the_engine() {
        let group = BudgetGroup(9);
        let (mut engine, mut restored) = pair(1 << 12, 4);

        // A log of what was applied and where, the way a real one would be read back.
        let mut log: Vec<(PendingEffect, ApplyIndex)> = Vec::new();
        let holds = RECORDS_PER_BLOCK * 6;
        for id in 1..=holds as u64 {
            let at = ApplyIndex(id);
            let effect = create(id as u128, group);
            engine.write(effect, at).expect("the index took the hold");
            log.push((effect, at));
        }
        // A settle of one member in full, which is the only way a group member may be resolved, and a
        // partial settle of a hold with no group, which is the shape that appends a new version.
        let mut next = holds as u64 + 1;
        for id in [1u128, 2, 3] {
            let at = ApplyIndex(next);
            let effect = PendingEffect::Remove {
                pending_ref: TxId(id),
                budget: group,
                released: 100,
            };
            engine.write(effect, at).expect("a removal frees a slot");
            log.push((effect, at));
            next += 1;
        }
        let lone = next as u128 + 1000;
        for effect in [
            create(lone, BudgetGroup::ABSENT),
            PendingEffect::Reduce {
                pending_ref: TxId(lone),
                debit_account: AccountId(1),
                credit_account: AccountId(2),
                amount: 100,
                remaining: 40,
                ledger: 1,
                budget: BudgetGroup::ABSENT,
            },
        ] {
            let at = ApplyIndex(next);
            engine.write(effect, at).expect("applied");
            log.push((effect, at));
            next += 1;
        }

        let covered = engine.coverage();
        assert!(
            covered.raw() > 0 && covered.raw() < next,
            "coverage {covered:?} is not between nothing and everything, so this proves little"
        );

        round_trip(&mut engine, &mut restored).expect("a snapshot of this engine");
        assert_eq!(
            restored.coverage(),
            covered,
            "the restored engine forgot where it stood"
        );

        // Everything after coverage, in order, the way recovery reads it.
        let reflects = engine.applied_through();
        for (effect, at) in log.iter().filter(|(_, at)| *at > covered) {
            restored
                .replay(*effect, *at, reflects)
                .expect("replay applied");
        }

        for id in 1..=next as u128 + 1000 {
            let before = engine.lookup(TxId(id)).map(|hold| {
                (
                    hold.remaining,
                    hold.budget_members,
                    hold.budget_remaining,
                    hold.debit_account,
                )
            });
            let after = restored.lookup(TxId(id)).map(|hold| {
                (
                    hold.remaining,
                    hold.budget_members,
                    hold.budget_remaining,
                    hold.debit_account,
                )
            });
            assert_eq!(before, after, "hold {id} differs after restore and replay");
        }
        assert!(
            restored.counts_agree(),
            "the restored table's counts do not add up"
        );
    }

    /// The one effect that is not idempotent on its own: a `Create` arriving again would give one key two
    /// slots, and then one `remove` clears one and the other survives — a resolved hold alive again, with its
    /// money reserved for good. `replay` turns it into a repoint; `write` does not, because that path cannot
    /// meet the case and finding out costs it a record read (see `Arrival`).
    #[test]
    fn a_create_arriving_again_repoints_rather_than_inserting_twice() {
        let (mut engine, _) = pair(1 << 12, 4);
        let effect = create(1, BudgetGroup(3));
        engine.write(effect, ApplyIndex(1)).expect("applied");
        let once = engine.lookup(TxId(1)).expect("the hold");

        // Told that the totals already reflect this position, which is what a restore's header says. Without
        // that the group would count the member twice, and it is the caller's to supply for exactly that
        // reason — see `PendingEngine::replay`.
        engine
            .replay(effect, ApplyIndex(1), ApplyIndex(1))
            .expect("replayed");
        let twice = engine.lookup(TxId(1)).expect("the hold is still one hold");
        assert_eq!(once.remaining, twice.remaining);
        assert_eq!(
            (once.budget_members, once.budget_remaining),
            (twice.budget_members, twice.budget_remaining),
            "the group counted the same member twice"
        );

        // And one removal is enough to end it, which is the failure a second slot would have caused.
        engine
            .write(
                PendingEffect::Remove {
                    pending_ref: TxId(1),
                    budget: BudgetGroup(3),
                    released: 100,
                },
                ApplyIndex(2),
            )
            .expect("applied");
        assert!(
            engine.lookup(TxId(1)).is_none(),
            "a second slot survived the removal"
        );
    }

    /// A snapshot reads the table as it was when it started, even while the engine keeps writing into it.
    ///
    /// **The kick cascade is what this is for**, not the effects. An entry displaced from one bucket to
    /// another mid-dump appears twice in the stream — and then one `remove` clears one slot, the other
    /// survives, and a resolved hold is alive again with its money reserved for good — or it appears nowhere,
    /// and no replay restores it because a relocation is in no log. The test writes enough between chunks to
    /// make the table relocate, and asserts the snapshot is still the one it began.
    #[test]
    fn a_snapshot_reads_the_table_it_began_with_while_the_engine_writes() {
        // A table at its load target, so inserts cascade rather than finding room.
        let slots = 1 << 10;
        let holds = (slots as f64 * crate::index::LOAD_TARGET) as u64 / 2;
        let (mut engine, mut restored) = pair(slots, 1);
        for id in 1..=holds {
            engine
                .write(create(id as u128, BudgetGroup::ABSENT), ApplyIndex(id))
                .expect("the index took the hold");
        }
        let mut writer = engine.begin_snapshot();
        let coverage = engine.coverage();
        let mut reader = SnapshotReader::new();
        let mut chunk = vec![0u8; CHUNK];
        let mut next = holds;
        let mut shadow_peak = 0;
        loop {
            let written = engine.next_snapshot_chunk(&mut writer, &mut chunk);
            if written == 0 {
                break;
            }
            reader
                .take_chunk(&chunk[..written], restored.index_mut())
                .expect("a stream this table can take");
            // Between chunks: more holds, which is what makes buckets move under the reader.
            for _ in 0..3 {
                next += 1;
                let _ = engine.write(create(next as u128, BudgetGroup::ABSENT), ApplyIndex(next));
            }
            shadow_peak = shadow_peak.max(engine.shadowed_buckets());
        }
        assert!(
            shadow_peak > 0,
            "nothing was written into a bucket the snapshot had not reached, so this proves nothing"
        );
        assert_eq!(
            engine.shadowed_buckets(),
            0,
            "the shadow outlived the snapshot that needed it"
        );
        assert!(reader.is_complete());
        restored.restore(reader.into_groups(), coverage);

        // Nothing lost: every position coverage claims is there. Coverage is a *lower* bound on what has been
        // sealed — it errs low on purpose — so the snapshot carrying more than it claims is right, and a hold
        // at or below it missing is the relocation that appeared nowhere.
        for id in 1..=coverage.raw() {
            assert!(
                restored.lookup(TxId(id as u128)).is_some(),
                "hold {id} is at or below coverage {} and was not carried",
                coverage.raw()
            );
        }

        // Nothing twice: one key cannot hold two slots. A lookup finds one of them, so counting the holds
        // that answer and comparing against the table's entries is what catches the relocation written twice.
        let found = (1..=next)
            .filter(|id| restored.lookup(TxId(*id as u128)).is_some())
            .count() as u64;
        assert_eq!(
            found,
            restored.traffic().index_live as u64,
            "the table has more entries than holds answer, so a relocation was written twice"
        );
        for id in (holds + 1)..=next {
            assert!(
                restored.lookup(TxId(id as u128)).is_none(),
                "hold {id} arrived after the snapshot began and was carried anyway"
            );
        }
        assert!(restored.counts_agree());
    }

    /// A snapshot only restores into a table of the same size, and a mismatch is refused rather than
    /// interpreted. A bucket's position in the stream *is* its position in the table — a slot holds a
    /// fingerprint and not a key, so nothing can be placed again — which makes the bucket count part of what
    /// the bytes mean.
    #[test]
    fn a_table_of_a_different_size_refuses_the_stream() {
        let mut engine = PendingEngine::sized(64, 1, 64, Box::new(MemoryStore::default()));
        engine
            .write(create(1, BudgetGroup::ABSENT), ApplyIndex(1))
            .expect("the index took the hold");

        let mut wrong = PendingEngine::sized(1 << 14, 1, 64, Box::new(MemoryStore::default()));
        let refused = round_trip(&mut engine, &mut wrong).expect_err("a table of another size");
        assert!(
            matches!(refused, NotASnapshot::Buckets { .. }),
            "refused for the wrong reason: {refused:?}"
        );
    }

    /// Bytes that are not a snapshot are refused, and so is a version this build does not know. Both are
    /// refusals rather than best-effort reads: a table restored from bytes that meant something else is
    /// worse than a table that was not restored.
    #[test]
    fn bytes_that_are_not_a_snapshot_are_refused() {
        let mut engine = PendingEngine::with_windows(1, 64);
        let junk = [0u8; RECORD];
        assert_eq!(
            SnapshotReader::new().take_chunk(&junk, engine.index_mut()),
            Err(NotASnapshot::Unrecognised)
        );

        let mut header = [0u8; RECORD];
        header[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        header[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(
            SnapshotReader::new().take_chunk(&header, engine.index_mut()),
            Err(NotASnapshot::Version(VERSION + 1))
        );

        let short = [0u8; RECORD - 1];
        assert_eq!(
            SnapshotReader::new().take_chunk(&short, engine.index_mut()),
            Err(NotASnapshot::Malformed)
        );
    }
}
