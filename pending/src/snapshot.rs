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
//! What is **not** here yet, and why:
//!
//! - **The coverage index.** A snapshot has to say which log position its state reflects, or replay does
//!   not know where to start. That number is the flush frontier's, and no component records the log position
//!   of anything yet — the seam is open (`ApplyIndex`) and deliberately not plumbed. It arrives with replay,
//!   because a coverage index with no replay to use it would be a number with nothing behind it.
//! - **A stable read.** Walking the index while the engine keeps writing gives a smeared view, and a kick
//!   cascade can move an entry between buckets so it appears twice or nowhere — §15 has the two failures.
//!   Copying the buckets about to change is the answer, and it belongs with pacing.
//! - **The group totals' own boundary.** A group whose members straddle the frontier should carry only what
//!   is sealed, and working that out needs the membership the engine does not index. Without coverage there
//!   is no frontier to straddle, so the whole map goes for now and the asymmetry is noted rather than hidden.

use ledger_base::{Amount, BudgetGroup, FxHashMap};

use crate::block::RecordLog;
use crate::engine::BudgetState;
use crate::index::{address_in, HoldTable};

/// Every record in the format, header included. Four eight-byte slots make a bucket exactly this wide, and
/// a group entry fits inside it, so the stream is a sequence of same-sized records and a chunk never splits
/// one.
pub const RECORD: usize = 32;

/// Little-endian by declaration and not by inheritance, for the same reason the block format is (§12): the
/// moment these bytes leave the process they are a format, and a format that borrows the machine's byte
/// order is not one.
const MAGIC: u64 = 0x5041_5f53_4e41_5031;

/// Bumped when the layout of any record changes. A reader that does not know a version refuses the stream
/// rather than interpreting it, because the alternative is a table restored from bytes that meant something
/// else.
const VERSION: u32 = 1;

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

/// Writes the engine's state out in chunks. Borrows what it walks, so nothing is copied to be written and a
/// caller cannot forget to keep the engine still — which the type says and the missing stable read does not
/// yet make true.
pub struct SnapshotWriter<'a> {
    index: &'a HoldTable,
    records: &'a RecordLog,
    groups: Vec<(BudgetGroup, BudgetState)>,
    /// Records already written, header included, so a chunk resumes rather than restarting.
    at: u64,
}

impl<'a> SnapshotWriter<'a> {
    pub fn new(
        index: &'a HoldTable,
        records: &'a RecordLog,
        budgets: &FxHashMap<BudgetGroup, BudgetState>,
    ) -> Self {
        // Sorted, so two writers over the same state produce the same bytes. A map's iteration order is not
        // a promise, and a snapshot that differed between nodes for no reason would be one nothing could
        // compare.
        let mut groups: Vec<(BudgetGroup, BudgetState)> =
            budgets.iter().map(|(id, state)| (*id, *state)).collect();
        groups.sort_by_key(|(id, _)| id.raw());
        Self {
            index,
            records,
            groups,
            at: 0,
        }
    }

    /// Records the whole stream holds: the header, one per bucket, one per group.
    pub fn records(&self) -> u64 {
        1 + self.index.bucket_count() as u64 + self.groups.len() as u64
    }

    pub fn bytes(&self) -> u64 {
        self.records() * RECORD as u64
    }

    /// Fills `into` with as many whole records as fit and answers how many bytes were written. Zero means
    /// the stream is finished, or that `into` was too small to hold one record.
    pub fn next_chunk(&mut self, into: &mut [u8]) -> usize {
        let mut written = 0;
        while written + RECORD <= into.len() {
            let Some(()) = self.write_one(&mut into[written..written + RECORD]) else {
                break;
            };
            written += RECORD;
            self.at += 1;
        }
        written
    }

    fn write_one(&self, into: &mut [u8]) -> Option<()> {
        let buckets = self.index.bucket_count() as u64;
        match self.at {
            0 => {
                into.fill(0);
                into[0..8].copy_from_slice(&MAGIC.to_le_bytes());
                into[8..12].copy_from_slice(&VERSION.to_le_bytes());
                into[12..20].copy_from_slice(&buckets.to_le_bytes());
                into[20..28].copy_from_slice(&(self.groups.len() as u64).to_le_bytes());
                Some(())
            }
            at if at <= buckets => {
                let words = self.index.bucket_words((at - 1) as usize);
                for (way, word) in words.iter().enumerate() {
                    // A slot pointing at a record that is not on a sealed block is written out empty. Its
                    // record is in the writeback buffer or in the block still being filled, so it will not
                    // be there on restore, and an index entry naming a block nobody has is worse than a hold
                    // the log can create again.
                    let keep = *word != 0 && self.records.is_sealed(address_in(*word));
                    let out = if keep { *word } else { 0 };
                    into[way * 8..way * 8 + 8].copy_from_slice(&out.to_le_bytes());
                }
                Some(())
            }
            at => {
                let (id, state) = self.groups.get((at - buckets - 1) as usize)?;
                into.fill(0);
                into[0..16].copy_from_slice(&id.raw().to_le_bytes());
                into[16..20].copy_from_slice(&state.members().to_le_bytes());
                into[20..28].copy_from_slice(&state.remaining().to_le_bytes());
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
    /// Records taken so far, header included.
    at: u64,
    groups: FxHashMap<BudgetGroup, BudgetState>,
}

impl SnapshotReader {
    pub fn new() -> Self {
        Self {
            buckets: 0,
            groups_expected: 0,
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
                self.groups_expected = u64_at(20);
                let ours = index.bucket_count() as u64;
                if self.buckets != ours {
                    return Err(NotASnapshot::Buckets {
                        theirs: self.buckets,
                        ours,
                    });
                }
                Ok(())
            }
            at if at <= self.buckets => {
                let words = [u64_at(0), u64_at(8), u64_at(16), u64_at(24)];
                match index.restore_bucket((at - 1) as usize, words) {
                    true => Ok(()),
                    false => Err(NotASnapshot::Malformed),
                }
            }
            at if at <= self.buckets + self.groups_expected => {
                let id = BudgetGroup(u128::from_le_bytes(
                    record[0..16].try_into().expect("16 bytes"),
                ));
                let members = u32_at(16);
                let remaining = u64_at(20) as Amount;
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
        self.at == 1 + self.buckets + self.groups_expected
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
    use ledger_base::ports::PendingEffect;
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::block::{BlockAddr, BlockStore, MemBlockStore, RECORDS_PER_BLOCK};
    use crate::engine::PendingEngine;

    /// A chunk small enough that every test here crosses several, because a format that is only ever read in
    /// one piece is not the format the engine will use.
    const CHUNK: usize = RECORD * 3;

    /// One block store two engines share, which is what a restore actually has: the snapshot carries the
    /// index and the blocks are already on the disk that survived. An engine restored over an empty store
    /// would find every slot and be able to read none of them, which is a test about nothing.
    #[derive(Clone, Default)]
    struct SharedStore(Rc<RefCell<MemBlockStore>>);

    impl BlockStore for SharedStore {
        fn write(&mut self, addr: BlockAddr, bytes: &[u8]) {
            self.0.borrow_mut().write(addr, bytes);
        }

        fn read(&self, addr: BlockAddr, into: &mut [u8]) -> bool {
            self.0.borrow().read(addr, into)
        }

        fn submit(&mut self, handle: u64, addr: BlockAddr, now: u64) -> bool {
            self.0.borrow_mut().submit(handle, addr, now)
        }

        fn poll(&mut self, now: u64, into: &mut [u8]) -> Option<u64> {
            self.0.borrow_mut().poll(now, into)
        }

        fn blocks(&self) -> usize {
            self.0.borrow().blocks()
        }

        fn inflight(&self) -> usize {
            self.0.borrow().inflight()
        }

        fn free_segment(&mut self, segment: u8) -> usize {
            self.0.borrow_mut().free_segment(segment)
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
    fn round_trip(from: &PendingEngine, into: &mut PendingEngine) -> Result<(), NotASnapshot> {
        let mut writer = from.snapshot();
        let mut reader = SnapshotReader::new();
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let written = writer.next_chunk(&mut chunk);
            if written == 0 {
                break;
            }
            reader.take_chunk(&chunk[..written], into.index_mut())?;
        }
        assert!(
            reader.is_complete(),
            "the stream ended before its header said"
        );
        into.restore_groups(reader.into_groups());
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
                .write(create(id as u128, group))
                .expect("the index took the hold");
        }

        round_trip(&engine, &mut restored).expect("a snapshot of this engine");

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
            .write(create(1, BudgetGroup::ABSENT))
            .expect("the index took the hold");
        assert!(
            engine.lookup(TxId(1)).is_some(),
            "the hold is not there to begin with"
        );

        round_trip(&engine, &mut restored).expect("a snapshot of this engine");

        assert!(
            restored.lookup(TxId(1)).is_none(),
            "an unflushed hold was carried, so the index names a block nobody has"
        );
        assert!(restored.counts_agree());
    }

    /// A snapshot only restores into a table of the same size, and a mismatch is refused rather than
    /// interpreted. A bucket's position in the stream *is* its position in the table — a slot holds a
    /// fingerprint and not a key, so nothing can be placed again — which makes the bucket count part of what
    /// the bytes mean.
    #[test]
    fn a_table_of_a_different_size_refuses_the_stream() {
        let mut engine = PendingEngine::sized(64, 1, 64, Box::new(MemBlockStore::default()));
        engine
            .write(create(1, BudgetGroup::ABSENT))
            .expect("the index took the hold");

        let mut wrong = PendingEngine::sized(1 << 14, 1, 64, Box::new(MemBlockStore::default()));
        let refused = round_trip(&engine, &mut wrong).expect_err("a table of another size");
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
