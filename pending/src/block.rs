use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use ledger_base::ports::{ApplyIndex, HoldData};
use ledger_base::{AccountId, Amount, BudgetGroup, FxHashMap, LedgerError, LineFit, Prng, TxId};
use ledger_stubkit::Server;

/// A block is what one read fetches, and the size the speed contract is written against: one hold
/// costs one block, whatever else that block happens to hold.
pub const BLOCK_BYTES: usize = 4096;

/// A record on a block: its key, then the hold. The key is stored because the index carries only a
/// 16-bit fingerprint, so the record is what tells two colliding keys apart.
pub const RECORD_BYTES: usize = 80;

/// Fifty-one, with the remainder of the block unused. The source design says sixty-four per block,
/// which needs its 128-byte record halved by compression; uncompressed that figure is thirty-two.
/// This record is packed rather than padded, so more fit than either — and the intra-block index is
/// six bits wide for the same reason it is there: sixty-four is the ceiling the format allows.
pub const RECORDS_PER_BLOCK: usize = BLOCK_BYTES / RECORD_BYTES;

/// One block's bytes, aligned to the block size.
///
/// The alignment is not the cache line's and not a throughput choice. Direct IO is how a real store has
/// to read and write — the residency window is already this engine's cache, so the page cache would be a
/// second copy of it — and it requires the buffer address, the offset and the length all aligned to the
/// device's own block. Offsets and lengths are whole blocks by construction; a `Vec<u8>` is aligned to
/// one byte, and an unaligned buffer costs a bounce copy per IO on a path that has none (rule 10). So the
/// alignment arrives with the buffer rather than with the backend that will need it.
///
/// Expressed here rather than through `cache_aligned!` because that macro funnels the *target's* line
/// size, which varies per build and is why it has to be written in one place. This alignment is
/// `BLOCK_BYTES` and cannot vary with the target; the assertion below is what keeps the literal
/// `repr(align(..))` demands from drifting away from it.
#[repr(align(4096))]
pub struct Block([u8; BLOCK_BYTES]);

const _: () = assert!(
    core::mem::align_of::<Block>() == BLOCK_BYTES,
    "a block's alignment is the block size, which is what direct IO requires"
);

ledger_base::layout_claim!(BLOCK_LAYOUT: Block, size = BLOCK_BYTES, LineFit::WholeLines);

/// Where a block's checksum sits: after the records, in the sixteen bytes fifty-one eighty-byte records
/// leave over. **Integrity costs no space at all here**, which is why it is a whole-block checksum and not a
/// per-record one — fifty-one four-byte stamps would not fit in sixteen bytes, and widening the record to
/// carry its own would drop the block from fifty-one records to forty-eight and cost six percent of the
/// store.
const CHECKSUM_AT: usize = RECORDS_PER_BLOCK * RECORD_BYTES;

impl Block {
    /// Seals the block's bytes with a checksum over its records.
    ///
    /// CRC32C from a crate rather than the hasher already in `base`: `rustc-hash` is built for placing keys in
    /// a table, and using it here because it is to hand would be picking the tool by availability. A CRC is
    /// the one that *guarantees* what this needs — every one-bit and two-bit error and every burst up to
    /// thirty-two bits detected, rather than a probability — and CRC32C is a hardware instruction on both
    /// supported targets.
    fn stamp(&mut self) {
        let checksum = crc32c::crc32c(&self.0[..CHECKSUM_AT]);
        self.0[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Whether these bytes are the ones that were written. False means silent corruption — a device that
    /// answered rather than refused — and it is the one failure this store could not previously see at all.
    fn intact(&self) -> bool {
        let stamped = u32::from_le_bytes(
            self.0[CHECKSUM_AT..CHECKSUM_AT + 4]
                .try_into()
                .expect("four bytes"),
        );
        stamped == crc32c::crc32c(&self.0[..CHECKSUM_AT])
    }

    /// Boxed rather than returned by value: blocks live in a `VecDeque` that moves its elements, and
    /// four kilobytes is not something to move.
    pub fn zeroed() -> Box<Self> {
        Box::new(Self([0; BLOCK_BYTES]))
    }

    pub fn copy_of(other: &Self) -> Box<Self> {
        let mut fresh = Self::zeroed();
        fresh.copy_from_slice(other);
        fresh
    }
}

impl Deref for Block {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl DerefMut for Block {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

const INDEX_BITS: u32 = 6;
/// Thirty-five, not thirty-six: the index slot spends its forty-eighth bit on saying whether a
/// fingerprint is shared, so an address has forty-seven. Four kilobytes times two to the thirty-five is
/// a hundred and thirty-seven terabytes, against the one the design sizes.
const BLOCK_BITS: u32 = 35;
/// Width of the segment field, and so how many values it can take. Exported because a per-segment array
/// is sized from it — the index keeps one live count per segment — and a length written beside the field
/// rather than derived from it is a length that can disagree with it.
pub const SEGMENT_BITS: u32 = 6;
pub const SEGMENT_VALUES: usize = 1 << SEGMENT_BITS;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
const BLOCK_MASK: u64 = (1 << BLOCK_BITS) - 1;

/// Where a record is: segment, block, and which record of the block. Forty-seven bits, which is what an
/// index slot has left beside its fingerprint and its ambiguity bit — the source design packs the same
/// three into forty and spends the difference on a narrower block field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordAddr(u64);

impl RecordAddr {
    pub const fn new(segment: u8, block: u64, index: u8) -> Self {
        Self(
            ((segment as u64) << (BLOCK_BITS + INDEX_BITS))
                | ((block & BLOCK_MASK) << INDEX_BITS)
                | (index as u64 & INDEX_MASK),
        )
    }

    pub const fn segment(self) -> u8 {
        (self.0 >> (BLOCK_BITS + INDEX_BITS)) as u8
    }

    pub const fn block(self) -> u64 {
        (self.0 >> INDEX_BITS) & BLOCK_MASK
    }

    pub const fn index(self) -> u8 {
        (self.0 & INDEX_MASK) as u8
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Where this record's block sits in its segment. **This is the whole of the layout rule and it lives
    /// here alone**: the block number times the block size, absolute rather than relative to whatever the
    /// segment's first block happened to be.
    ///
    /// Absolute is what leaves nothing to restore. Block numbers count on across day boundaries, so a
    /// segment's file begins with a hole and its own blocks are one extent inside it — and the extent map
    /// a filesystem already keeps *is* the layout, so a restart derives every offset from the address and
    /// asks nobody. The relative form needed the segment's first block, which is not a function of the
    /// live slots — a leading block whose records all died leaves no slot to find it by — so it would have
    /// had to travel in the snapshot, and §15's whole argument is that the snapshot is the index and the
    /// rest is derived. A restore proved it rather than argued it: the test that shares a store between
    /// two engines could not read a single record.
    ///
    /// What it costs is an apparent file size, not space: allocation is the day's blocks and nothing else.
    pub const fn block_offset(self) -> u64 {
        self.block() * BLOCK_BYTES as u64
    }
}

/// Little-endian, stated rather than inherited: these bytes are a format the moment the blocks leave
/// this process, and a format that borrows the machine's byte order is not one.
pub fn encode(key: TxId, hold: &HoldData, into: &mut [u8]) {
    debug_assert_eq!(into.len(), RECORD_BYTES);
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        into[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(&key.raw().to_le_bytes());
    put(&hold.debit_account.raw().to_le_bytes());
    put(&hold.credit_account.raw().to_le_bytes());
    put(&hold.amount.to_le_bytes());
    put(&hold.remaining.to_le_bytes());
    put(&hold.ledger.to_le_bytes());
    put(&hold.budget.raw().to_le_bytes());
    put(&hold.budget_members.to_le_bytes());
    put(&hold.budget_remaining.to_le_bytes());
    debug_assert_eq!(at, RECORD_BYTES);
}

pub fn decode(bytes: &[u8], _from: RecordAddr) -> (TxId, HoldData) {
    debug_assert_eq!(bytes.len(), RECORD_BYTES);
    let mut at = 0;
    let mut take = |width: usize| {
        let taken = &bytes[at..at + width];
        at += width;
        taken
    };
    let u128_at = |taken: &[u8]| u128::from_le_bytes(taken.try_into().expect("16 bytes"));
    let u64_at = |taken: &[u8]| u64::from_le_bytes(taken.try_into().expect("8 bytes"));
    let u32_at = |taken: &[u8]| u32::from_le_bytes(taken.try_into().expect("4 bytes"));
    let key = TxId(u128_at(take(16)));
    let hold = HoldData {
        debit_account: AccountId(u64_at(take(8))),
        credit_account: AccountId(u64_at(take(8))),
        amount: u64_at(take(8)) as Amount,
        remaining: u64_at(take(8)) as Amount,
        ledger: u32_at(take(4)),
        budget: BudgetGroup(u128_at(take(16))),
        budget_members: u32_at(take(4)),
        budget_remaining: u64_at(take(8)) as Amount,
    };
    (key, hold)
}

/// What a store can fail at. Two, and they are different in kind even though the reaction is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFault {
    /// The store is asked for a block it does not have. Not a miss — the index only ever names blocks that
    /// were written — so it is this node's own record of where blocks are having stopped agreeing with the
    /// store.
    Missing,
    /// The device refused: an `EIO`, or an `ENOSPC` on a store whose size retention was supposed to bound.
    /// Our bookkeeping is intact and the block is not there anyway, which is why the reaction is the same
    /// one — a hold this node cannot read is a resolution it cannot answer.
    Device,
}

/// Whole blocks at a segment and an offset, written once and freed a segment at a time. Memory backs it
/// today; a file or a network volume goes underneath without the engine above changing.
///
/// **The vocabulary here is a filesystem's, and that is what this seam is for.** A segment is a file: it
/// is brought into being by its first block, appended to after that, and removed whole. Offsets and
/// lengths are whole blocks, which is what direct IO asks of them alongside the buffer's alignment. And
/// the *caller* turns an address into an offset — `RecordAddr::block_offset`, which needs no state to do
/// it — so nothing below here needs to know what a block number is, let alone what a hold is. A backend is
/// then seven methods: a file per segment, an extent per segment on a raw device, or memory.
///
/// Two ways to read, because the engine has two callers with different constraints. A **lookup** submits
/// and harvests, so a store with a latency does not stop the loop. **Applying** a committed decision cannot
/// wait for anything — it is in order — so it reads synchronously, and a store that models a device charges
/// that read to the thread rather than to its queue (`take_charge`). Both are priced now; they are counted
/// separately because only the second is what a read cache would remove.
pub trait DurableStore {
    /// A segment's first block, which is what brings the segment into being. One call rather than a
    /// create followed by a write: a segment's first block *is* its creation, and two statements that
    /// always happen together are two that can come apart (rule 16). The caller knows which of this and
    /// `append` a write is, from the blocks it has already put there, so a backend never pays a syscall
    /// to find out what its caller already knew.
    fn open_with(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault>;
    /// The next block of a segment that already exists. Same shape as `open_with` on purpose: the two
    /// differ in whether the segment is there yet and in nothing else, and which it is is the caller's to
    /// say.
    fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault>;
    /// `&mut` although reading changes nothing a caller can see: a store that models a device charges
    /// the read, and one that can fail counts it.
    fn read_at(&mut self, segment: u8, offset: u64, into: &mut Block) -> Result<(), StoreFault>;
    /// False when the store will not take another read yet, which is backpressure rather than failure.
    fn submit(&mut self, handle: u64, segment: u8, offset: u64, now: u64) -> bool;
    /// The next read finished by `now`, copied out. `None` while nothing is due.
    fn poll(&mut self, now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>>;
    fn inflight(&self) -> usize;
    /// Device time this store's *synchronous* calls have cost since it was last asked, taken as it is read.
    /// A write, a sync and an apply-path read hold the thread the way a real `pwrite`, `fsync` and `pread`
    /// do, and on the pending engine's thread that time is every lookup's latency as well.
    ///
    /// A charge rather than a wait, because whoever runs the loop is the only thing that has a clock, and on
    /// a virtual one a wait that only time can end never ends. Zero from a store that models no device.
    fn take_charge(&mut self) -> u64 {
        0
    }
    /// Everything written before this returns is durable when it does.
    ///
    /// **One call with no argument, and that is a property of a filesystem rather than a simplification.**
    /// `fsync(fd)` makes a file's bytes durable, but a file that has just been created also needs its
    /// directory synced or a crash can leave durable bytes in a file that does not exist. So durability is
    /// a fact about the store at a moment, not a watermark per segment — which is the optimisation someone
    /// would otherwise reach for. What is covered is the caller's to remember, because the caller is what
    /// wrote it.
    fn sync(&mut self) -> Result<(), StoreFault>;
    /// Stops a segment existing. The one way the store shrinks: blocks are written once and never
    /// rewritten, so space comes back a whole day at a time, and only once nothing in the index points
    /// into that day.
    ///
    /// It answers nothing about how many blocks there were. The caller wrote them and so already knows,
    /// and a real store could not answer anyway — `unlink` does not count what it removes.
    fn remove(&mut self, segment: u8) -> Result<(), StoreFault>;
}

/// The exact store: it keeps what it was given and adds no latency. Every other store is measured
/// against this one, and a simulation that wants a device's tail wraps it rather than replacing it.
///
/// A segment's blocks are one growing sequence rather than a map keyed by address, and that is a property
/// being proved rather than a detail of the stand-in: **where a block sits follows from its offset
/// alone**, which is what a file requires and what a map let this code get away without. Boxed per block
/// so growing the sequence moves pointers rather than four kilobytes at a time — physical contiguity is
/// what a file has and what nothing here rests on; the offsets are.
pub struct MemoryStore {
    segments: [Option<SegmentFile>; SEGMENT_VALUES],
    /// Submitted reads, answered in the order they were asked for and with no delay. A store that
    /// modelled a device would answer out of order; this one is the baseline that says what the
    /// structure does when the device is not the variable.
    submitted: VecDeque<(u64, u8, u64)>,
}

/// One segment's blocks, and the offset the first of them landed at.
///
/// The hole in front of that offset is what a sparse file has and this does not have to: `base` is the
/// whole of it. A real backend keeps no equivalent — the filesystem's extent map is already this — which is
/// why nothing has to restore it and why memory is allowed to forget it.
struct SegmentFile {
    base: u64,
    /// `None` is a block this segment has a place for and was never given, which happens for one reason: a
    /// write the store refused. The caller advances its block number anyway — it must, because the records on
    /// the block it could not write already hold addresses, and reusing the number would give two records one
    /// address — so the next block lands past the end and leaves a hole.
    ///
    /// A file behaves differently here and it is worth knowing which: reading a hole in a file gives zeroes
    /// rather than an error, so there the block fails its checksum and is counted as corruption. Same seal,
    /// different cause, and both only after a write already failed.
    blocks: Vec<Option<Box<Block>>>,
}

impl SegmentFile {
    fn at(&self, offset: u64) -> Option<&Block> {
        let within = offset.checked_sub(self.base)?;
        self.blocks
            .get((within / BLOCK_BYTES as u64) as usize)?
            .as_deref()
    }

    fn end(&self) -> u64 {
        self.base + self.blocks.len() as u64 * BLOCK_BYTES as u64
    }

    /// Puts a block at an offset at or past the end, leaving `None` for anything skipped.
    fn put(&mut self, offset: u64, block: &Block) {
        let at = ((offset - self.base) / BLOCK_BYTES as u64) as usize;
        while self.blocks.len() < at {
            self.blocks.push(None);
        }
        self.blocks.push(Some(Block::copy_of(block)));
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            segments: std::array::from_fn(|_| None),
            submitted: VecDeque::new(),
        }
    }
}

impl MemoryStore {
    fn block_at(&self, segment: u8, offset: u64) -> Result<&Block, StoreFault> {
        debug_assert!(
            offset.is_multiple_of(BLOCK_BYTES as u64),
            "an offset is a whole number of blocks, which is what direct IO requires of it"
        );
        self.segments[segment as usize]
            .as_ref()
            .and_then(|file| file.at(offset))
            .ok_or(StoreFault::Missing)
    }
}

impl DurableStore for MemoryStore {
    fn open_with(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        // What `O_EXCL` is for, and a self-invariant rather than a fault: both sides of it are ours. A
        // segment brought into being twice would hold two days' blocks under one day's count.
        debug_assert!(
            self.segments[segment as usize].is_none(),
            "a segment is brought into being once, by its first block"
        );
        self.segments[segment as usize] = Some(SegmentFile {
            base: offset,
            blocks: vec![Some(Block::copy_of(block))],
        });
        Ok(())
    }

    fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        let file = self.segments[segment as usize]
            .as_mut()
            .ok_or(StoreFault::Missing)?;
        // At the end, or past it. Never before it: blocks are written once, so an offset already occupied is
        // the caller's block numbers and this sequence disagreeing, which is a self-invariant rather than the
        // store being broken. Past the end is a hole left by a write this store refused — the caller advances
        // its block number whether or not the write landed, because the records on the block it could not
        // write already hold addresses.
        debug_assert!(
            offset >= file.end(),
            "a block was written over one this segment already has"
        );
        file.put(offset, block);
        Ok(())
    }

    fn read_at(&mut self, segment: u8, offset: u64, into: &mut Block) -> Result<(), StoreFault> {
        into.copy_from_slice(self.block_at(segment, offset)?);
        Ok(())
    }

    fn submit(&mut self, handle: u64, segment: u8, offset: u64, _now: u64) -> bool {
        self.submitted.push_back((handle, segment, offset));
        true
    }

    fn poll(&mut self, _now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        let (handle, segment, offset) = self.submitted.pop_front()?;
        Some(self.read_at(segment, offset, into).map(|()| handle))
    }

    fn inflight(&self) -> usize {
        self.submitted.len()
    }

    /// Nothing to do, and nothing dishonest about that: memory has no second layer to push bytes into. What
    /// this store implements is the *barrier* — the caller learns what is covered by when it asked — and
    /// that half is the half a test can exercise without a device.
    fn sync(&mut self) -> Result<(), StoreFault> {
        Ok(())
    }

    fn remove(&mut self, segment: u8) -> Result<(), StoreFault> {
        // One drop, which is what `unlink` costs. What this replaced went looking through a map for the
        // blocks of a day, and that was a stand-in's cost rather than a store's: the sweep bench had to
        // leave the round that frees a day out of its numbers because it was the worst round at every
        // size and hid the one being measured.
        self.segments[segment as usize] = None;
        Ok(())
    }
}

/// The one segment that is on no disk: an address in it is a record still in the writeback buffer,
/// waiting to be flushed. Segments are days and thirty-four are ever live, so the top of the six-bit
/// field is free for this — and making the two forms distinguishable is what lets the buffer hand out
/// addresses before it knows where a record will end up.
pub const BUFFER_SEGMENT: u8 = (SEGMENT_VALUES - 1) as u8;

/// Segments a stored record can be in: every value of the six-bit field but the one above. A segment is
/// a day and its number is the day modulo this, so it is also the ceiling on how many days of records can
/// be live at once — past it two live segments would share a number and expiry would drop the wrong day's
/// records. `MemoryPendingConfig::validate` refuses a lifetime that reaches it.
pub const SEGMENTS: u64 = BUFFER_SEGMENT as u64;

impl RecordAddr {
    pub const fn buffered(ordinal: u64, index: u8) -> Self {
        Self::new(BUFFER_SEGMENT, ordinal, index)
    }

    pub const fn is_buffered(self) -> bool {
        self.segment() == BUFFER_SEGMENT
    }
}

/// The blocks one day wrote: where its first went and how many followed. Empty until the day seals its
/// first block, which is also the state a freed day goes back to.
///
/// A range rather than a list of block numbers, and that is a property of the format rather than a
/// convenience: block numbers count on across day boundaries and a day's records are sealed while that day
/// is the open one, so a day's blocks are consecutive by construction.
#[derive(Debug, Clone, Copy, Default)]
struct BlockRange {
    first: u64,
    blocks: u64,
}

impl BlockRange {
    fn note(&mut self, block: u64) {
        if self.blocks == 0 {
            self.first = block;
        }
        self.blocks += 1;
    }

    fn block_at(&self, at: u64) -> Option<u64> {
        (at < self.blocks).then_some(self.first + at)
    }
}

/// One block's worth of records, and how much of it is used.
struct Filling {
    bytes: Box<Block>,
    filled: usize,
    /// The log position of the batch whose effect put the first record here. Only meaningful while the block
    /// is in the writeback buffer, which is the one place it is read: the oldest buffered block's position is
    /// where a snapshot's coverage has to stop, because everything from that batch onwards has not reached a
    /// block yet. Recycled buffers reset it with `filled`.
    began_at: ApplyIndex,
}

impl Filling {
    fn new() -> Self {
        Self {
            bytes: Block::zeroed(),
            filled: 0,
            began_at: ApplyIndex::default(),
        }
    }

    fn full(&self) -> bool {
        self.filled == RECORDS_PER_BLOCK
    }

    fn put_at(&mut self, key: TxId, hold: &HoldData, at: ApplyIndex) -> usize {
        if self.filled == 0 {
            self.began_at = at;
        }
        self.put(key, hold)
    }

    fn put(&mut self, key: TxId, hold: &HoldData) -> usize {
        let at = self.filled * RECORD_BYTES;
        encode(key, hold, &mut self.bytes[at..at + RECORD_BYTES]);
        self.filled += 1;
        self.filled - 1
    }

    fn get(&self, index: usize, addr: RecordAddr) -> Option<(TxId, HoldData)> {
        if index >= self.filled {
            return None;
        }
        let at = index * RECORD_BYTES;
        Some(decode(&self.bytes[at..at + RECORD_BYTES], addr))
    }
}

/// How the store behaves, as a stand-in charges it. Every nanos field zero is the exact store — memory, no
/// delay — which is what every other answer is measured against. Anything else wraps it in a device's timing,
/// because there is no disk under this yet and pretending otherwise in silence would be worse than saying so.
///
/// It lives beside the store rather than beside the engine's configuration, because it describes the store.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreModel {
    pub read_base_nanos: u64,
    /// The mean of the tail. A fixed latency completes every read in the order it was asked for and hides
    /// what putting a lane back in order costs.
    pub read_tail_nanos: u64,
    /// What sealing one block costs. A write is synchronous and on the engine's own thread, so this is time
    /// no lookup gets — see `busy_until`.
    pub write_base_nanos: u64,
    pub write_tail_nanos: u64,
    /// What making the written blocks durable costs. The one the sync cadence turns on: an `fsync` is the
    /// longest thing a real store does synchronously, and this thread is the one every lookup passes through.
    pub sync_base_nanos: u64,
    pub sync_tail_nanos: u64,
    /// Reads a second the device can serve, zero for no ceiling. Reads only: writes and syncs are serialised
    /// by holding the thread, which is a rate of its own and needs no second gate.
    pub iops: u64,
    /// Reads it will hold at once. Past this the engine keeps the command and asks again.
    pub queue_depth: usize,
    /// Fail every nth call the store is given, as a device would. A fault, and the only reason this exists:
    /// the reaction to a store that will not do as it is told is rule 19's seal, and a seal nothing can
    /// produce is a seal nothing has tested.
    pub fault_every: u32,
    /// Flip a bit in every nth block the store answers a read with. The other way a device misbehaves, and
    /// the worse one: it says yes. Before there was a checksum this was not a fault but a wrong answer, and
    /// the whole point of the knob is that the wrong answer is now a seal.
    pub corrupt_every: u32,
}

/// What is under the store, opened and checked before anything is spawned. A directory that cannot be used is
/// a configuration error and has to be refused where somebody can be told, not on a worker thread whose only
/// way to report it would be to panic (rule 6) — so this carries the directory already open.
///
/// Not `Copy`, which is why it travels beside `DaySource` rather than inside `MemoryPendingConfig`: both are
/// things the engine is handed from outside rather than sizes it derives.
pub enum OpenBacking {
    /// Memory. The exact store, and what every other answer is measured against.
    Memory,
    /// One file per segment, in a directory that has been opened.
    Files { dir: std::fs::File, path: PathBuf },
}

impl OpenBacking {
    /// The directory to put a segment's files in, opened and created if it was not there.
    pub fn files(path: &Path) -> Result<Self, LedgerError> {
        let (dir, path) =
            crate::files::open_directory(path).map_err(|_| LedgerError::ConfigInvalid)?;
        Ok(Self::Files { dir, path })
    }
}

impl StoreModel {
    pub fn build(&self, backing: OpenBacking, seed: u64) -> Box<dyn DurableStore> {
        let exact: Box<dyn DurableStore> = match backing {
            OpenBacking::Memory => Box::new(MemoryStore::default()),
            OpenBacking::Files { dir, path } => Box::new(crate::files::FileStore::new(
                dir,
                path,
                self.queue_depth.max(1),
            )),
        };
        if self.is_exact() {
            return exact;
        }
        Box::new(LatencyStore::new(exact, *self, seed))
    }

    fn is_exact(&self) -> bool {
        self.read_base_nanos == 0
            && self.read_tail_nanos == 0
            && self.write_base_nanos == 0
            && self.write_tail_nanos == 0
            && self.sync_base_nanos == 0
            && self.sync_tail_nanos == 0
            && self.iops == 0
            && self.fault_every == 0
            && self.corrupt_every == 0
    }
}

/// What one synchronous call to the device costs: a floor and an exponential tail, the same shape the read
/// queue draws from. Zero for both is a call that costs nothing, which is how a model with only reads set
/// stays exactly what it was.
#[derive(Debug, Clone, Copy, Default)]
struct Cost {
    base_nanos: u64,
    tail_nanos: u64,
}

impl Cost {
    fn draw(&self, prng: &mut Prng) -> u64 {
        if self.base_nanos == 0 && self.tail_nanos == 0 {
            return 0;
        }
        self.base_nanos + prng.exponential_nanos(self.tail_nanos)
    }
}

/// A store with a device's timing in front of another store. What makes it a stand-in is
/// `stubkit::Server` — a rate gate with an exponential tail, so reads are admitted no faster than the
/// ceiling and each draws its own latency, which is what makes them complete **out of the order they
/// were asked for**. It lives here rather than in `stubkit` only because the trait does and the
/// dependency runs this way.
///
/// **Two kinds of cost, because two different things are being occupied.** A lookup's read occupies the
/// *device*: it is submitted, the queue serves it, and the engine keeps working — a deadline per read, and
/// completions come back out of order. A write, a sync and the apply path's read occupy the *thread*: a real
/// `pwrite` or `fsync` blocks, and on this thread that is every lookup's latency as well. So those three are
/// charged to `busy_until` and the round that ran them does nothing more until the clock passes it.
///
/// A charge rather than a wait, and that is what makes it work where a wait would not: on a virtual clock a
/// wait only time can end never ends. It also removes the one thing this used to be unable to answer — the
/// apply path's read latency was priced as IO and left unmodelled, because there was nowhere to put it.
///
/// **It wraps any store, including one with a real device under it, and the composition is a floor rather
/// than a sum.** Every cost is turned into an absolute time from when the call was admitted, so when the
/// inner store is memory the drawn time is the whole of it, and when the inner store is real its own time has
/// already passed by the time the deadline is compared — whichever is slower wins, with nothing measuring
/// the difference. That is what makes "model a device slower than the one I have" mean something, and
/// modelling a faster one impossible.
pub struct LatencyStore {
    inner: Box<dyn DurableStore>,
    device: Server,
    read: Cost,
    write: Cost,
    sync: Cost,
    prng: Prng,
    /// Reads handed to the store below and not yet answered by it: handle, segment, offset, and when this
    /// model says the device is done with them.
    inflight: Vec<(u64, u8, u64, u64)>,
    /// Reads the store below *has* answered, waiting for this model's own time to pass: handle, due, bytes.
    ///
    /// The bytes have to be held, and there is no way around it: the store below answers in its order and
    /// this model releases in its own, so a completion that arrives early has to be kept somewhere. Bounded
    /// by `queue_depth` — half a megabyte at the default and eight at 2048 — which is a measurement tool's
    /// cost and is stated rather than hidden.
    completed: Vec<(u64, u64, Box<Block>)>,
    /// Buffers of completions already released, so the steady state allocates nothing.
    spare: Vec<Box<Block>>,
    queue_depth: usize,
    /// Device time charged by synchronous calls and not yet handed to whoever has a clock.
    charged_nanos: u64,
    fault_every: u32,
    corrupt_every: u32,
    calls: u64,
    reads: u64,
}

impl LatencyStore {
    pub fn new(inner: Box<dyn DurableStore>, model: StoreModel, seed: u64) -> Self {
        let queue_depth = model.queue_depth.max(1);
        Self {
            inner,
            device: Server::new(model.read_base_nanos, model.read_tail_nanos, model.iops),
            read: Cost {
                base_nanos: model.read_base_nanos,
                tail_nanos: model.read_tail_nanos,
            },
            write: Cost {
                base_nanos: model.write_base_nanos,
                tail_nanos: model.write_tail_nanos,
            },
            sync: Cost {
                base_nanos: model.sync_base_nanos,
                tail_nanos: model.sync_tail_nanos,
            },
            prng: Prng::new(seed),
            inflight: Vec::with_capacity(queue_depth),
            completed: Vec::with_capacity(queue_depth),
            spare: Vec::new(),
            queue_depth,
            charged_nanos: 0,
            fault_every: model.fault_every,
            corrupt_every: model.corrupt_every,
            calls: 0,
            reads: 0,
        }
    }

    /// Flips one bit of a block this store has just answered a read with, if this is the read the fault
    /// takes. One bit and in a record rather than in the checksum, because a stamp that fails to match its
    /// own bytes is the easy case: the interesting one is bytes that changed and a stamp that did not.
    fn corrupt(&mut self, block: &mut Block) {
        self.reads += 1;
        if self.corrupt_every == 0 || !self.reads.is_multiple_of(u64::from(self.corrupt_every)) {
            return;
        }
        let at = (self.prng.next_u64() % CHECKSUM_AT as u64) as usize;
        block[at] ^= 1 << (self.prng.next_u64() % 8);
    }

    fn charge(&mut self, cost: Cost) {
        self.charged_nanos += cost.draw(&mut self.prng);
    }

    /// Whether this call is the one the fault takes. Counted over every call rather than per kind, because a
    /// device that is failing is not failing at one method.
    fn refuses(&mut self) -> bool {
        self.calls += 1;
        self.fault_every > 0 && self.calls.is_multiple_of(u64::from(self.fault_every))
    }
}

impl DurableStore for LatencyStore {
    fn open_with(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        self.charge(self.write);
        if self.refuses() {
            return Err(StoreFault::Device);
        }
        self.inner.open_with(segment, offset, block)
    }

    fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        self.charge(self.write);
        if self.refuses() {
            return Err(StoreFault::Device);
        }
        self.inner.append(segment, offset, block)
    }

    /// The read that cannot be submitted and harvested: the apply path is in order and cannot park a
    /// decision half way, and the expiry walk reads a whole block at a time. Both hold the thread, so both
    /// are charged to it rather than to the device's queue.
    fn read_at(&mut self, segment: u8, offset: u64, into: &mut Block) -> Result<(), StoreFault> {
        self.charge(self.read);
        if self.refuses() {
            return Err(StoreFault::Device);
        }
        self.inner.read_at(segment, offset, into)?;
        self.corrupt(into);
        Ok(())
    }

    /// Handed down as well as timed, which it was not before.
    ///
    /// **The store below has to do the read, because whatever concurrency it has is the thing being
    /// measured.** This used to record a deadline and then read synchronously when the deadline passed, which
    /// was indistinguishable while every backing read synchronously anyway — and would have silently bypassed
    /// a backing with a thread pool, so the one measurement a modelled latency exists for (how many threads a
    /// rate needs, without a device) would have said nothing.
    ///
    /// The queue depth is this model's, and it counts what is held either side of the store below: refusing
    /// here is what a device with a full queue does.
    fn submit(&mut self, handle: u64, segment: u8, offset: u64, now: u64) -> bool {
        if self.inflight.len() + self.completed.len() >= self.queue_depth {
            return false;
        }
        // The store below first: a deadline recorded for a read it would not take is a read this would
        // release having never done it (rule 17).
        if !self.inner.submit(handle, segment, offset, now) {
            return false;
        }
        let due = self.device.serve(now, &mut self.prng);
        self.inflight.push((handle, segment, offset, due));
        true
    }

    /// A completion leaves when the store below has answered it **and** this model's time for it has passed —
    /// the later of the two, which is what makes the composition a floor rather than a sum. A device modelled
    /// slower than the one underneath dominates; one modelled faster cannot be expressed, which is correct.
    fn poll(&mut self, now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        // Everything the store below has finished, moved across with the time this model gave it.
        loop {
            let mut buffer = self.spare.pop().unwrap_or_else(Block::zeroed);
            match self.inner.poll(now, &mut buffer) {
                None => {
                    self.spare.push(buffer);
                    break;
                }
                Some(Err(fault)) => {
                    self.spare.push(buffer);
                    return Some(Err(fault));
                }
                Some(Ok(handle)) => {
                    let Some(at) = self
                        .inflight
                        .iter()
                        .position(|(asked, ..)| *asked == handle)
                    else {
                        // A completion for a read this did not submit. Nothing to time it against, so it is
                        // handed straight up rather than held for a deadline that does not exist.
                        into.copy_from_slice(&buffer);
                        self.spare.push(buffer);
                        return Some(Ok(handle));
                    };
                    let (.., due) = self.inflight.swap_remove(at);
                    self.completed.push((handle, due, buffer));
                }
            }
        }
        let at = self.completed.iter().position(|(_, due, _)| *due <= now)?;
        let (handle, _, buffer) = self.completed.swap_remove(at);
        if self.refuses() {
            self.spare.push(buffer);
            return Some(Err(StoreFault::Device));
        }
        into.copy_from_slice(&buffer);
        self.spare.push(buffer);
        self.corrupt(into);
        Some(Ok(handle))
    }

    fn inflight(&self) -> usize {
        self.inflight.len() + self.completed.len()
    }

    fn sync(&mut self) -> Result<(), StoreFault> {
        self.charge(self.sync);
        if self.refuses() {
            return Err(StoreFault::Device);
        }
        self.inner.sync()
    }

    fn take_charge(&mut self) -> u64 {
        std::mem::take(&mut self.charged_nanos)
    }

    /// Freeing costs the device nothing this model charges for: it is off any request's path, and a device
    /// that made it expensive would be one whose extents this store does not model.
    fn remove(&mut self, segment: u8) -> Result<(), StoreFault> {
        self.inner.remove(segment)
    }
}

/// The records: a writeback buffer of recent blocks, the blocks flushed out of it that are still worth
/// keeping in memory, and behind both the store. Append-only throughout — a hold whose remainder changed
/// is written again at a new address and the index is repointed, because a block that could be rewritten
/// would cost a read before every write and would take with it the one property store addresses rest on:
/// that they never move.
///
/// A block leaves the buffer by being **compacted**: only the records the index still points at are
/// written on, packed together with survivors of earlier blocks, and their index entries follow them.
/// Without that the store would grow with holds created rather than holds alive, which is the figure
/// the whole capacity estimate rests on. Buffered addresses are therefore provisional, and that is why
/// they are a segment of their own.
///
/// **Two windows, and they are not the same window.** Flushing is what bounds recovery: a record that has
/// not reached the store exists only in memory and has to be in the checkpoint, so the buffer is short.
/// Residency is what keeps IO off the resolutions that come soon after — a flushed block stays readable
/// in memory long after its content is durable. Written and resident are independent states, and holding
/// them apart is what lets the first window be an hour while the second is a day. Residency costs far
/// less than a day of arrivals, because what is resident has already been compacted: the survivors.
pub struct RecordLog {
    store: Box<dyn DurableStore>,
    /// Recent blocks, oldest first; the last is the one being filled. Not on the store yet.
    buffer: VecDeque<Filling>,
    /// Ordinal of `buffer.front()`, so an address stays unique after blocks are flushed away.
    oldest: u64,
    /// Blocks the buffer may hold before its oldest is compacted out — the flush window. A count, not a
    /// duration: the engine has no clock, so a window in time is this divided by a rate.
    flush_blocks: usize,
    /// Survivors accumulate here so a store block is packed rather than one-per-flush. Written out
    /// when full.
    store_open: Filling,
    /// Blocks already written to the store and kept in memory anyway, oldest first — the residency
    /// window. Dropping one frees memory and loses nothing: the store has it.
    resident: VecDeque<Filling>,
    /// Block number of `resident.front()`.
    oldest_resident: u64,
    resident_blocks: usize,
    /// Blocks dropped out of residency, whose buffers the next seal reuses. Bounded by the window, and
    /// it makes the steady state — drop one, seal one — allocate nothing.
    spare: Vec<Filling>,
    segment: u8,
    /// The blocks each day wrote, so expiry can read a day's own records instead of searching the index
    /// for them. Block numbers count on across day boundaries, so a day owns a contiguous range and two
    /// numbers describe it — which is the property that makes this a pair of counters rather than a list.
    days: [BlockRange; SEGMENT_VALUES],
    next_block: u64,
    /// The oldest block sealed since the last sync, and the log position it began at. `None` when everything
    /// sealed is durable.
    ///
    /// One pair rather than a list, because seals are in block order and a sync covers all of them at once:
    /// the oldest is the only one either answer needs. It is what separates *written* from *durable* on this
    /// side of the seam, which is where the separation has to live — the store is asked for a barrier and
    /// remembers nothing, because what a barrier covered is known only to whoever wrote it.
    unsynced: Option<(u64, ApplyIndex)>,
    /// Read into, so a lookup allocates nothing.
    scratch: Box<Block>,
    /// Reads asked of the store and not yet answered, by handle, because a block carries several
    /// records and only the address says which one was wanted.
    fetching: FxHashMap<u64, RecordAddr>,
    appended: u64,
    died_in_buffer: u64,
    flushed: u64,
    /// Blocks handed back to the store, which is the only way it shrinks.
    freed: u64,
    /// Faults the store has reported, and whether one is still owed to whoever can act on it.
    ///
    /// Latched rather than returned up the call stack, and the one round of delay is deliberate: a seal
    /// belongs to the sequencer, the paths that meet a fault are three (a write inside compaction, an
    /// apply-path read, a harvested completion), and threading a `Result` out of all three would put the
    /// same decision in three places. Rule 19 is still what happens — nothing is answered from a faulted
    /// read, and the seal follows on the next round.
    faults: u64,
    /// Blocks the store answered with whose checksum did not match. Counted apart from a refusal because it
    /// is the opposite behaviour — a device that answered rather than one that said no — and because before
    /// there was a checksum it was not a fault at all but a wrong answer.
    corruptions: u64,
    fault_owed: bool,
    left_memory: u64,
    buffer_reads: u64,
    resident_reads: u64,
    store_reads: u64,
    apply_store_reads: u64,
    inflight_peak: usize,
}

impl Default for RecordLog {
    fn default() -> Self {
        Self::new(
            Box::new(MemoryStore::default()),
            DEFAULT_FLUSH_BLOCKS,
            DEFAULT_RESIDENT_BLOCKS,
        )
    }
}

/// Enough that a workload which resolves most holds quickly resolves them before their block is
/// compacted, and small enough that a test can fill it. What these should be in a deployment follows
/// from the declared business inputs — see `PendingCapacity` — not from here.
pub const DEFAULT_FLUSH_BLOCKS: usize = 1024;
/// Larger than the flush window, because that is the whole point of there being two.
pub const DEFAULT_RESIDENT_BLOCKS: usize = 4096;

impl RecordLog {
    pub fn new(store: Box<dyn DurableStore>, flush_blocks: usize, resident_blocks: usize) -> Self {
        let mut buffer = VecDeque::new();
        buffer.push_back(Filling::new());
        Self {
            store,
            buffer,
            oldest: 0,
            flush_blocks: flush_blocks.max(1),
            store_open: Filling::new(),
            resident: VecDeque::new(),
            oldest_resident: 0,
            resident_blocks,
            spare: Vec::new(),
            segment: 0,
            days: [BlockRange::default(); SEGMENT_VALUES],
            next_block: 0,
            unsynced: None,
            scratch: Block::zeroed(),
            fetching: FxHashMap::default(),
            appended: 0,
            died_in_buffer: 0,
            flushed: 0,
            freed: 0,
            faults: 0,
            corruptions: 0,
            fault_owed: false,
            left_memory: 0,
            buffer_reads: 0,
            resident_reads: 0,
            store_reads: 0,
            apply_store_reads: 0,
            inflight_peak: 0,
        }
    }

    /// Writes the record into the buffer and answers where it went. The address is provisional until
    /// the block is compacted.
    pub fn append(&mut self, key: TxId, hold: &HoldData, at: ApplyIndex) -> RecordAddr {
        if self.buffer.back().is_some_and(Filling::full) {
            self.buffer.push_back(Filling::new());
        }
        let ordinal = self.oldest + self.buffer.len() as u64 - 1;
        let index = self
            .buffer
            .back_mut()
            .expect("a block to fill")
            .put_at(key, hold, at);
        self.appended += 1;
        RecordAddr::buffered(ordinal, index as u8)
    }

    /// Whether the buffer is over its flush window and its oldest block is due to be compacted.
    pub fn over_window(&self) -> bool {
        self.buffer.len() > self.flush_blocks
    }

    /// Which day the records being written now belong to. A segment is a day, and its number is the day
    /// modulo the segments the address field has — unambiguous because only a lifetime's worth of days is
    /// ever live, which `MemoryPendingConfig::validate` is what guarantees.
    pub fn segment(&self) -> u8 {
        self.segment
    }

    /// Hands a day's blocks back, and answers how many. The caller decides *when*: only once nothing in
    /// the index points into that day, which is the one moment they are known to be dead.
    ///
    /// The count comes from this day's range rather than from the store, because the store no longer has
    /// one to give: it stops a segment existing, and a real one's `unlink` cannot say what it removed. The
    /// blocks were noted here as they were written, so this side already knows.
    ///
    /// Residency is not touched, and it does not have to be: it holds the most recently written blocks,
    /// and a day old enough to be freed left it long before. A configuration that kept records in memory
    /// longer than they are allowed to exist is refused at startup rather than handled here.
    pub fn free_segment(&mut self, segment: u8) -> usize {
        let freed = self.days[segment as usize].blocks as usize;
        // Nothing to react to: the reaction to a store that cannot do as it is told is rule 19's, and it
        // arrives with a store that can fail at it.
        let _ = self.store.remove(segment);
        self.freed += freed as u64;
        self.days[segment as usize] = BlockRange::default();
        freed
    }

    /// Records that the store refused, whatever it refused. One counter and one flag: the reaction does not
    /// vary with which call met it, because every one of them means a record this node cannot read.
    fn note_fault(&mut self) {
        self.faults += 1;
        self.fault_owed = true;
    }

    /// Whether what the store just read into the scratch buffer is what was written. Every path that reads a
    /// block goes through this, and none of them decodes anything when it says no: a record built out of
    /// bytes that changed under us is a wrong answer, which is worse than the seal that follows.
    fn scratch_intact(&mut self) -> bool {
        if self.scratch.intact() {
            return true;
        }
        self.corruptions += 1;
        self.fault_owed = true;
        false
    }

    /// Whether a fault is owed to whoever can act on it, taken as it is read.
    pub fn take_fault(&mut self) -> bool {
        std::mem::take(&mut self.fault_owed)
    }

    /// Device time the store's synchronous calls have cost since this was last asked. Whoever has a clock
    /// turns it into a deadline of its own: the charge has one owner and each driver has its own time.
    pub fn take_store_charge(&mut self) -> u64 {
        self.store.take_charge()
    }

    /// Makes durable everything sealed since this was last asked, and answers whether there was anything to
    /// do. The caller decides how often; the consequence of waiting is that coverage lags, never that
    /// anything is lost, because what is not durable is still in the log.
    pub fn sync(&mut self) -> bool {
        if self.unsynced.is_none() {
            return false;
        }
        // Nothing to react to yet, for the same reason a write has nothing: no store here can refuse. The
        // reaction — a coverage that must stop advancing, and rule 19 if it keeps failing — arrives with a
        // store that can fail at it.
        if self.store.sync().is_ok() {
            self.unsynced = None;
        } else {
            // Coverage stops advancing on its own — `unsynced` is what it stops at and it is still set — so
            // a snapshot cannot claim a block this failed to make durable. The seal is for the same reason
            // as a failed write: a device refusing is one this node cannot go on writing to.
            self.note_fault();
        }
        true
    }

    /// The last log position everything up to which has reached a **durable** block, given the position of
    /// the batch being applied now.
    ///
    /// Three things can be short of durable and they are checked oldest first: a block sealed and not yet
    /// synced, the block being filled towards the store, and the oldest block in the writeback buffer. The
    /// answer is the first one's own position minus one, because it holds the first record a crash would not
    /// find — so a snapshot claiming to cover its batch would be claiming a record it does not carry. With
    /// none of the three, everything applied is durable and the answer is the caller's own position.
    ///
    /// **Sealed used to be the test, and it was the wrong one the moment the store had a `sync`.** A block
    /// handed to a store is written, not durable; the two are the same event only in memory. Erring here is
    /// one-sided: stopping too early costs replay a little, and stopping too late claims a record that is
    /// gone.
    ///
    /// Position zero means it covers nothing, which is a legitimate answer rather than a missing one: a
    /// snapshot of an engine that has applied nothing is what a follower starting from empty receives.
    pub fn durable_through(&self, applied_through: ApplyIndex) -> ApplyIndex {
        // Oldest first, and the order is by construction rather than by comparison: a sealed-and-unsynced
        // block was drained out of `store_open`, which is drained out of the buffer, so each one's stamp is
        // at or before the next one's.
        //
        // The block being filled has to be in this list at all, and that was a real defect: coverage claimed
        // a hundred and fifty-three while the records of position a hundred and three sat in it, so the
        // snapshot left their slots out and replay started after them. The holds were simply gone. Its stamp
        // comes from the buffered block compaction drained into it, which is a lower bound on its survivors'
        // own positions — conservative in the safe direction.
        if let Some((_, began_at)) = self.unsynced {
            return ApplyIndex(began_at.raw().saturating_sub(1));
        }
        let unsealed = [
            &self.store_open,
            self.buffer.front().unwrap_or(&self.store_open),
        ]
        .into_iter()
        .find(|block| block.filled > 0);
        match unsealed {
            Some(block) => ApplyIndex(block.began_at.raw().saturating_sub(1)),
            None => applied_through,
        }
    }

    /// Whether this address names a record on a block a crash would still find. False for a record in the
    /// writeback buffer, false for one in the block still being filled — that block has handed out addresses
    /// and has not been written — and false for one on a block written but not yet synced.
    ///
    /// A snapshot asks it about every slot it keeps. An index entry naming a block nobody has is worse than a
    /// hold the log can create again, so a slot pointing anywhere but a durable block is written out empty.
    pub fn is_durable(&self, addr: RecordAddr) -> bool {
        let durable_blocks = match self.unsynced {
            Some((first, _)) => first,
            None => self.next_block,
        };
        !addr.is_buffered() && addr.block() < durable_blocks
    }

    /// Blocks this day wrote, which is also how many freeing it hands back — the store cannot say, so this
    /// side is where the number is.
    pub fn blocks_in_day(&self, segment: u8) -> u64 {
        self.days[segment as usize].blocks
    }

    /// Every record of one of a day's blocks, by position within the day. `false` means the day has no
    /// block there, which is how a walk knows it has reached the end.
    ///
    /// One store read for the whole block, and it does not try memory first. A day being expired is
    /// `retention + grace` days old and a configuration is refused unless residency is shorter than that,
    /// so its blocks left memory long ago — the same reason `free_segment` does not touch residency. Asking
    /// memory would be a test whose answer is always no.
    ///
    /// A closure rather than an iterator because the block is decoded out of the log's own scratch buffer,
    /// so nothing is allocated per record or per block. The address goes with each record because that is
    /// what the caller checks the index against — a record is alive exactly when the index still points at
    /// it, which is the same test compaction makes.
    pub fn each_record_in_day(
        &mut self,
        segment: u8,
        at: u64,
        visit: &mut dyn FnMut(TxId, HoldData, RecordAddr),
    ) -> bool {
        let Some(block) = self.days[segment as usize].block_at(at) else {
            return false;
        };
        // Where an address with its record field zeroed used to be built, which was the distinction between
        // a block and a record being made by convention rather than by the seam.
        let offset = block * BLOCK_BYTES as u64;
        if self
            .store
            .read_at(segment, offset, &mut self.scratch)
            .is_err()
        {
            // The range says this block was written, so the store not having it is this node's own
            // bookkeeping disagreeing with itself. Nothing here can repair it; the walk goes on and the
            // day's count is what refuses to reach zero.
            return true;
        }
        if !self.scratch_intact() {
            // Offering a void built from bytes that changed would release money against a record nobody
            // wrote. The day stays unfinished, which is the safe direction, and the seal stops the rest.
            return true;
        }
        self.store_reads += 1;
        for index in 0..RECORDS_PER_BLOCK {
            let addr = RecordAddr::new(segment, block, index as u8);
            let from = index * RECORD_BYTES;
            let (key, hold) = decode(&self.scratch[from..from + RECORD_BYTES], addr);
            if key != TxId::ABSENT {
                visit(key, hold, addr);
            }
        }
        true
    }

    /// Moves to a new day. The open store block is sealed first: it was promised addresses in the old
    /// segment, and a block whose records straddled two segments could not be deleted by either.
    ///
    /// Block numbers keep counting across the boundary rather than restarting, so an address stays unique
    /// on the block field alone and everything that finds a block by number — residency, the store —
    /// needs no notion of segments at all. The segment field is then purely the label saying which day a
    /// record belongs to, which is what expiry deletes by.
    pub fn open_day(&mut self, day: u64) {
        let segment = (day % SEGMENTS) as u8;
        if segment == self.segment {
            return;
        }
        if self.store_open.filled > 0 {
            self.seal_store_block();
        }
        self.segment = segment;
    }

    /// The records of the oldest buffered block, for the caller to sort into survivors and casualties.
    /// Borrowed rather than taken, so the index still resolves them while that is decided.
    pub fn oldest_block(&self) -> impl Iterator<Item = (TxId, HoldData, RecordAddr)> + '_ {
        let ordinal = self.oldest;
        let block = self.buffer.front();
        (0..RECORDS_PER_BLOCK).filter_map(move |index| {
            let addr = RecordAddr::buffered(ordinal, index as u8);
            block?.get(index, addr).map(|(key, hold)| (key, hold, addr))
        })
    }

    /// Writes a survivor on towards the store and answers its lasting address. The block it lands in
    /// is written out once full, so survivors of several buffered blocks share one — a block per flush
    /// would multiply the space a tenth of the records occupy.
    pub fn keep(&mut self, key: TxId, hold: &HoldData, from: ApplyIndex) -> RecordAddr {
        if self.store_open.full() {
            self.seal_store_block();
        }
        let index = self.store_open.put_at(key, hold, from);
        self.flushed += 1;
        RecordAddr::new(self.segment, self.next_block, index as u8)
    }

    /// The position the oldest buffered block began at, for compaction to carry on to the block it drains
    /// into. A lower bound on the positions of that block's survivors, which is what coverage wants: erring
    /// low costs replay a little and cannot claim a record is sealed that is not.
    pub fn oldest_began_at(&self) -> ApplyIndex {
        self.buffer
            .front()
            .map(|block| block.began_at)
            .unwrap_or_default()
    }

    /// Drops the oldest buffered block. Everything in it that mattered has been kept by now.
    pub fn drop_oldest(&mut self, died: usize) {
        if self.buffer.len() > 1 {
            self.buffer.pop_front();
            self.oldest += 1;
            self.died_in_buffer += died as u64;
        }
    }

    /// The record at an address if it is still in memory: the buffer, the store block being filled, or a
    /// flushed block still inside the residency window. `None` means only the store has it.
    ///
    /// Three places rather than one because they are three different claims — not yet written, being
    /// written, written and kept — and a report that could not tell the second window from the first
    /// could not say which one to widen.
    pub fn try_read(&mut self, addr: RecordAddr) -> Option<(TxId, HoldData)> {
        let index = addr.index() as usize;
        if addr.is_buffered() {
            self.buffer_reads += 1;
            let slot = addr.block().checked_sub(self.oldest)? as usize;
            return self.buffer.get(slot)?.get(index, addr);
        }
        // No segment check: block numbers count on across day boundaries, so the number alone says
        // where a block is, and the two bounds below are what decide whether it is still in memory. A
        // segment check here would send yesterday's resident blocks to the store while they sat in
        // memory.
        if addr.block() == self.next_block {
            self.buffer_reads += 1;
            return self.store_open.get(index, addr);
        }
        let slot = addr.block().checked_sub(self.oldest_resident)? as usize;
        let block = self.resident.get(slot)?;
        self.resident_reads += 1;
        block.get(index, addr)
    }

    /// The record at an address, wherever it lives, waiting for the store if it has to. Only the apply
    /// path uses this: it applies in order and cannot park a decision half way.
    pub fn read(&mut self, addr: RecordAddr) -> Option<(TxId, HoldData)> {
        if let Some(found) = self.try_read(addr) {
            return Some(found);
        }
        self.apply_store_reads += 1;
        self.read_from_store(addr)
    }

    fn read_from_store(&mut self, addr: RecordAddr) -> Option<(TxId, HoldData)> {
        self.store_reads += 1;
        if self
            .store
            .read_at(addr.segment(), addr.block_offset(), &mut self.scratch)
            .is_err()
        {
            self.note_fault();
            return None;
        }
        if !self.scratch_intact() {
            return None;
        }
        let at = addr.index() as usize * RECORD_BYTES;
        Some(decode(&self.scratch[at..at + RECORD_BYTES], addr))
    }

    /// Asks the store for the block this address is on. False is backpressure: the store will not take
    /// another read yet.
    pub fn fetch(&mut self, handle: u64, addr: RecordAddr, now: u64) -> bool {
        if !self
            .store
            .submit(handle, addr.segment(), addr.block_offset(), now)
        {
            return false;
        }
        self.fetching.insert(handle, addr);
        self.inflight_peak = self.inflight_peak.max(self.store.inflight());
        true
    }

    /// The next fetch finished by `now`. Completions may arrive in an order the reads were not asked
    /// in, which is what the orderer exists for.
    pub fn harvest(&mut self, now: u64) -> Option<(u64, RecordAddr, TxId, HoldData)> {
        let completed = self.store.poll(now, &mut self.scratch)?;
        let Ok(handle) = completed else {
            // The lookup that asked will never be answered, and that is the honest outcome rather than a
            // gap: its lane stalls, the seal below stops anything more being applied, and a drain that
            // never completes is what says to replace this leader (rule 19).
            self.note_fault();
            return None;
        };
        if !self.scratch_intact() {
            return None;
        }
        let addr = self.fetching.remove(&handle)?;
        self.store_reads += 1;
        let at = addr.index() as usize * RECORD_BYTES;
        let (key, hold) = decode(&self.scratch[at..at + RECORD_BYTES], addr);
        Some((handle, addr, key, hold))
    }

    /// Records appended, records that never left the buffer because they were resolved first, records
    /// carried on to the store, and reads answered from memory against reads that went to the store.
    /// The second is the number the design's capacity rests on and the one its own inputs disagree
    /// about.
    pub fn traffic(&self) -> LogTraffic {
        LogTraffic {
            appended: self.appended,
            died_in_buffer: self.died_in_buffer,
            flushed: self.flushed,
            left_memory: self.left_memory,
            buffer_reads: self.buffer_reads,
            resident_reads: self.resident_reads,
            store_reads: self.store_reads,
            apply_store_reads: self.apply_store_reads,
            inflight_peak: self.inflight_peak,
            freed: self.freed,
            store_faults: self.faults,
            store_corruptions: self.corruptions,
            ..LogTraffic::default()
        }
    }

    #[cfg(test)]
    pub fn appended(&self) -> u64 {
        self.appended
    }

    pub fn windows(&self) -> (usize, usize) {
        (self.flush_blocks, self.resident_blocks)
    }

    /// Blocks not written yet, blocks written and still in memory, and blocks in the store. The first
    /// two are what a checkpoint and a memory budget respectively have to cover, and the third is the
    /// only one that would not be memory once the store is a disk.
    pub fn blocks(&self) -> (usize, usize, usize) {
        (
            self.buffer.len(),
            self.resident.len() + usize::from(self.store_open.filled > 0),
            // Summed from the ranges rather than asked of the store, which no longer counts what it holds:
            // it is told a segment and an offset, and a real one's answer to "how many blocks have you"
            // would be a `stat` per file for a number this side already has.
            self.days.iter().map(|day| day.blocks as usize).sum(),
        )
    }

    /// The block is written now — not durable, which is `sync`'s to say — and it stays readable anyway: the
    /// two memory windows are independent, and this is the one place that says so. Residency is trimmed here
    /// rather than on a schedule because this is the only event that adds to it.
    fn seal_store_block(&mut self) {
        // Stamped here and nowhere else: this is the one moment a block's bytes stop changing, which is what
        // makes a whole-block checksum possible at all.
        self.store_open.bytes.stamp();
        let addr = RecordAddr::new(self.segment, self.next_block, 0);
        // A segment's first block is what brings it into being, and a later one lands in a segment that
        // exists. This side knows which from the day's own count, so the store is told rather than asked —
        // a real one would otherwise pay a syscall to find out what its caller already knew.
        let opening = self.days[self.segment as usize].blocks == 0;
        let written = if opening {
            self.store
                .open_with(self.segment, addr.block_offset(), &self.store_open.bytes)
        } else {
            self.store
                .append(self.segment, addr.block_offset(), &self.store_open.bytes)
        };
        // Nothing here can repair a store that would not take a block, and nothing here decides what to do
        // about it either: the index already points at these records, so the reaction is rule 19's seal and
        // it arrives with a store that can actually fail.
        if written.is_err() {
            // The index already points at every record on this block, so they are unreachable and the log
            // says they exist. Nothing here can repair that; the sequencer seals.
            self.note_fault();
        }
        // The oldest block a sync has not covered, and the position it began at. Only the first seal since a
        // sync records it: later ones are newer, and it is the oldest that bounds both coverage and which
        // slots a snapshot may keep.
        self.unsynced
            .get_or_insert((self.next_block, self.store_open.began_at));
        self.days[self.segment as usize].note(self.next_block);
        let fresh = self.spare.pop().unwrap_or_else(Filling::new);
        let sealed = std::mem::replace(&mut self.store_open, fresh);
        self.resident.push_back(sealed);
        self.next_block += 1;
        self.store_open.filled = 0;
        while self.resident.len() > self.resident_blocks {
            let Some(mut dropped) = self.resident.pop_front() else {
                break;
            };
            self.left_memory += dropped.filled as u64;
            self.oldest_resident += 1;
            dropped.filled = 0;
            self.spare.push(dropped);
        }
    }
}

/// What the log has done, for a report that has to say where the reads went.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogTraffic {
    pub appended: u64,
    pub died_in_buffer: u64,
    pub flushed: u64,
    /// Records that fell out of the residency window, so only the store has them now. A read of one of
    /// these is the IO the window exists to prevent.
    pub left_memory: u64,
    /// Blocks handed back once nothing in the index pointed into their day. The only way the store shrinks,
    /// and what makes its size a steady state rather than a total.
    pub freed: u64,
    /// Times the store refused a call. Non-zero means the apply path has been sealed, or is about to be:
    /// every one of them is a record this node cannot read and the log says exists.
    pub store_faults: u64,
    /// Blocks the store answered with whose checksum did not match — a device that lied rather than refused.
    /// The same seal, and a separate number because the two say different things about the hardware.
    pub store_corruptions: u64,
    pub buffer_reads: u64,
    /// Reads answered from a block that is on the store already and still in memory. The second window's
    /// whole return, and zero would mean it is not earning its size.
    pub resident_reads: u64,
    pub store_reads: u64,
    /// Store reads on the path that applies committed decisions, which cannot wait for them. The number
    /// a read cache would remove.
    pub apply_store_reads: u64,
    /// The most reads the store held at once — the queue depth a device would have to serve.
    pub inflight_peak: usize,
    /// Live entries against the slots the table was sized with, and the longest kick cascade seen. Both
    /// lengthen before inserts start failing, which is what makes "the table does not grow" safe.
    pub index_live: usize,
    pub index_slots: usize,
    pub worst_cascade: u32,
    /// Holds whose fingerprint turned out to be shared, and inserts the table could not take at all.
    pub ambiguous: u64,
    pub overflowed: u64,
    /// The day records are being written into, as a segment number, and how many expired days are still
    /// waiting to be emptied. One is ordinary. More than `grace_days` is the throttle behind by longer
    /// than the slack the index was sized with, and past that a hold cannot be stored and the node seals —
    /// so this is where "deleting late is safe" stops being true, and a run has to be able to see it.
    pub segment: u8,
    pub days_behind: u64,
    /// Days the sweep may still fall behind before the calendar stops moving. Zero means it has stopped —
    /// the day being written would otherwise come to share a segment with a day not yet emptied. Reported
    /// rather than derived by the reader, because the formula it follows from is the address format's and a
    /// report that made the reader know it would be a second place the rule lives.
    pub days_of_slack: u64,
    /// Blocks of expiring days the sweep has read. The sweep's whole cost, and a bounded one: a round reads
    /// the blocks it was asked for, and they are the day's own rather than the index's. The number this
    /// replaces was index slots walked, which nothing bounded.
    pub swept_blocks: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(amount: Amount) -> HoldData {
        HoldData {
            debit_account: AccountId(11),
            credit_account: AccountId(22),
            amount,
            remaining: amount - 1,
            ledger: 3,
            budget: BudgetGroup(99),
            budget_members: 2,
            budget_remaining: 44,
        }
    }

    /// A block the store answered with one bit changed is not decoded, and that is the whole of what the
    /// checksum buys: before it, those bytes became a `HoldData` and the answer was wrong rather than
    /// refused. Double-entry does not catch it — a corrupted remainder moves both sides by the same wrong
    /// amount — so nothing downstream would ever have said so.
    #[test]
    fn a_block_that_came_back_changed_is_refused_rather_than_decoded() {
        let model = StoreModel {
            // Every read, so the test does not depend on how many the log happens to issue.
            corrupt_every: 1,
            queue_depth: 8,
            ..StoreModel::default()
        };
        let mut log = RecordLog::new(model.build(OpenBacking::Memory, 7), 1, 0);
        // Past the window with survivors carried on, so a block is sealed and residency keeps none of it:
        // only the store has it, which is the one path a device can lie on.
        let mut addrs = Vec::new();
        for index in 0..RECORDS_PER_BLOCK * 3 {
            log.append(TxId(index as u128 + 1), &hold(10), ApplyIndex(1));
        }
        for index in 0..RECORDS_PER_BLOCK * 2 {
            addrs.push(log.keep(TxId(index as u128 + 1), &hold(10), ApplyIndex(1)));
        }
        let sealed = addrs[0];
        assert!(
            log.try_read(sealed).is_none(),
            "the block is still in memory, so this would not have reached the store"
        );
        assert!(
            log.read(sealed).is_none(),
            "a block whose checksum did not match was decoded anyway"
        );
        assert!(log.take_fault(), "the corruption was not owed to anyone");
        assert_eq!(log.traffic().store_corruptions, 1);
        assert_eq!(
            log.traffic().store_faults,
            0,
            "a device that answered wrongly was counted as one that refused"
        );
    }

    /// A modelled read leaves at the later of the two times: when the store below has answered it, and when
    /// this model says the device would have. Both directions, because only one of them was ever true.
    ///
    /// The second direction is the one that matters and the one that used to be missing: the model handed the
    /// read down not at all and read synchronously instead, so a backing with any concurrency of its own — a
    /// thread pool, io_uring — would have been bypassed, and the measurement a modelled latency exists for
    /// would have been measuring the model against itself.
    #[test]
    fn a_modelled_read_waits_for_the_later_of_the_two_times() {
        let slow_model = StoreModel {
            read_base_nanos: 1_000,
            queue_depth: 4,
            ..StoreModel::default()
        };
        let mut modelled = LatencyStore::new(Box::new(MemoryStore::default()), slow_model, 1);
        let block = Block::zeroed();
        modelled
            .open_with(0, 0, &block)
            .expect("memory takes a block");
        let mut into = Block::zeroed();

        assert!(modelled.submit(7, 0, 0, 0));
        assert!(
            modelled.poll(0, &mut into).is_none(),
            "the model released a read at once, so its own time did not gate anything"
        );
        assert_eq!(modelled.poll(1_000, &mut into), Some(Ok(7)));

        // And the other way: a model that costs nothing over a store below that is slow releases when the
        // store below does. Nested, because memory is the only backing here that answers instantly.
        let inner = Box::new(LatencyStore::new(
            Box::new(MemoryStore::default()),
            slow_model,
            2,
        ));
        let free = StoreModel {
            queue_depth: 4,
            // Not `default()`: every field zero is the exact store and would not be wrapped at all.
            iops: 1_000_000_000,
            ..StoreModel::default()
        };
        let mut stacked = LatencyStore::new(inner, free, 3);
        stacked
            .open_with(0, 0, &block)
            .expect("memory takes a block");
        assert!(stacked.submit(9, 0, 0, 0));
        assert!(
            stacked.poll(0, &mut into).is_none(),
            "the outer model released a read the store below had not answered"
        );
        assert_eq!(stacked.poll(1_000, &mut into), Some(Ok(9)));
    }

    /// Every field, both ways. A format that loses one field silently is a store that answers with
    /// someone else's hold, and the judge's checks would pass on whatever survived.
    #[test]
    fn a_record_survives_the_round_trip_field_for_field() {
        let mut bytes = vec![0u8; RECORD_BYTES];
        encode(TxId(1 << 100), &hold(500), &mut bytes);
        let (key, back) = decode(&bytes, RecordAddr::new(1, 2, 3));
        assert_eq!(key, TxId(1 << 100));
        assert_eq!(back.debit_account, AccountId(11));
        assert_eq!(back.credit_account, AccountId(22));
        assert_eq!((back.amount, back.remaining), (500, 499));
        assert_eq!(back.ledger, 3);
        assert_eq!(back.budget, BudgetGroup(99));
        assert_eq!((back.budget_members, back.budget_remaining), (2, 44));
    }

    #[test]
    fn an_address_carries_its_three_parts() {
        let addr = RecordAddr::new(63, (1 << BLOCK_BITS) - 1, 63);
        assert_eq!(
            (addr.segment(), addr.block(), addr.index()),
            (63, (1 << BLOCK_BITS) - 1, 63)
        );
        let modest = RecordAddr::new(2, 1_000_000, 7);
        assert_eq!(
            (modest.segment(), modest.block(), modest.index()),
            (2, 1_000_000, 7)
        );
        assert_eq!(RecordAddr::from_raw(modest.raw()), modest);
    }

    /// A record comes back from the block it landed in, whether that block is still being filled or
    /// has been sealed and handed to the store. Reading only one of the two would answer that a hold
    /// written moments ago does not exist.
    #[test]
    fn records_come_back_from_open_and_sealed_blocks_alike() {
        let mut log = RecordLog::default();
        let count = RECORDS_PER_BLOCK * 3 + 2;
        let addrs: Vec<RecordAddr> = (0..count)
            .map(|index| {
                log.append(
                    TxId(index as u128 + 1),
                    &hold(index as Amount + 1),
                    ApplyIndex(index as u64 + 1),
                )
            })
            .collect();
        let (buffered, ..) = log.blocks();
        assert!(buffered >= 4, "three filled blocks and one being filled");
        for (index, addr) in addrs.iter().enumerate() {
            let (key, back) = log.read(*addr).expect("a record that was appended");
            assert_eq!(key, TxId(index as u128 + 1), "wrong record at {addr:?}");
            assert_eq!(back.amount, index as Amount + 1);
        }
    }
}
