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

/// What a store names. A day's blocks are one object and the snapshot's two files are two more, and they
/// share a namespace because they share a disk (§20).
///
/// **A segment is not the store's word for it.** The day ↔ segment mapping is `RecordLog`'s and stays
/// there; below this the store is a device with objects on it, one of which happens to hold a day. That is
/// the whole of why this type exists: while a file was named by `segment: u8` there was nowhere for a
/// snapshot to be written that was not a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectId(u8);

impl ObjectId {
    /// The blocks of one day. The segment field is six bits wide, so this cannot reach the two names
    /// above it — asserted rather than left to that, because what keeps them apart is a fact about a
    /// different type (rule 18).
    pub const fn segment(segment: u8) -> Self {
        debug_assert!(
            (segment as usize) < SEGMENT_VALUES,
            "a segment number past the six-bit field would wear one of the snapshot's names"
        );
        Self(segment)
    }

    /// The snapshot a restart reads, and the one being written. Two names is what makes the replacement
    /// atomic — a dump is written to the second and renamed over the first (§19).
    pub const SNAPSHOT_CURRENT: Self = Self(SEGMENT_VALUES as u8);
    pub const SNAPSHOT_PARTIAL: Self = Self(SEGMENT_VALUES as u8 + 1);

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Objects a store may be asked for: every segment, and the snapshot's two. What a per-object array is
/// sized from, so a length cannot disagree with the namespace it indexes.
pub const OBJECT_VALUES: usize = SEGMENT_VALUES + 2;

/// Who asked for an IO, carried in the top bits of its handle so one store serving two callers can hand
/// each completion back to the one that asked for it.
///
/// **The tag is what makes a shared volume possible at all.** Two callers on one store draw handles from
/// counters of their own, so the numbers collide; a completion queue is one queue, so the poller has to be
/// able to tell whose each answer is. Deciding it once here is rule 18 — the alternative is each reader
/// guessing from whether it recognises the number, which is how a completion for somebody else gets
/// silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOwner {
    Blocks,
    Snapshot,
    /// The expiry sweep's block reads. Its own tag because they share the read queue with the lookups and
    /// a completion has to be told apart from theirs — the same reason the write side has two.
    Sweep,
}

const OWNER_SHIFT: u32 = 56;

impl IoOwner {
    /// The handle a sequence number of this owner's gets.
    pub const fn handle(self, sequence: u64) -> u64 {
        ((self as u64) << OWNER_SHIFT) | sequence
    }

    pub const fn owns(self, handle: u64) -> bool {
        handle >> OWNER_SHIFT == self as u64
    }
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

/// What one volume has done, counted by the volume.
///
/// **Every other IO number in this engine is a caller's tally of what it asked for**, which answers "what
/// did the drain do" and never "what is this disk doing". A volume is where reads and writes from every
/// caller meet, so it is the only place that can answer the second — and the only place a hung IO could
/// be noticed, which is where the watchdog goes when its reaction is chosen (§20).
///
/// Counted by each backing rather than by a layer above them, because only the backing knows whether a
/// submit was taken. What counts as what lives here, in the methods, so the rule is in one place even
/// where the calls are not (rule 1).
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeStats {
    /// Reads handed to the queue, and the ones it has answered.
    pub reads_submitted: u64,
    pub reads_answered: u64,
    /// Reads done on the calling thread instead: the apply-path fallback and the expiry sweep.
    pub reads_inline: u64,
    /// Reads answered from blocks already in hand rather than from the device, and reads that waited on
    /// one already on its way down instead of asking for the same block again — see `Cached`. Counted
    /// apart because they are the only numbers that can justify either on a machine whose page cache makes
    /// the reads they remove cheap anyway.
    pub reads_cached: u64,
    pub reads_joined: u64,
    pub writes: u64,
    pub barriers: u64,
    pub removes: u64,
    pub renames: u64,
    pub bytes_written: u64,
    /// Calls the volume would not take, by side. Backpressure rather than failure — the caller keeps the
    /// work — but a number that climbs is a volume the ledger is outrunning.
    pub reads_refused: u64,
    pub writes_refused: u64,
    /// The deepest each queue got, which is what says whether the declared depth is the binding constraint.
    pub read_depth_peak: usize,
    pub write_depth_peak: usize,
    /// Calls the volume itself failed, counted here as well as by whoever asked: on a volume two callers
    /// share, each of them sees only its own.
    pub faults: u64,
}

impl VolumeStats {
    pub(crate) fn took_read(&mut self, depth: usize) {
        self.reads_submitted += 1;
        self.read_depth_peak = self.read_depth_peak.max(depth);
    }

    pub(crate) fn answered_read(&mut self, ok: bool) {
        self.reads_answered += 1;
        if !ok {
            self.faults += 1;
        }
    }

    pub(crate) fn took_write(&mut self, bytes: usize, depth: usize) {
        self.writes += 1;
        self.bytes_written += bytes as u64;
        self.write_depth_peak = self.write_depth_peak.max(depth);
    }

    pub(crate) fn took_barrier(&mut self, depth: usize) {
        self.barriers += 1;
        self.write_depth_peak = self.write_depth_peak.max(depth);
    }

    pub(crate) fn answered_write(&mut self, ok: bool) {
        if !ok {
            self.faults += 1;
        }
    }

    /// Both counts a volume's own, so a caller adding its numbers to a model's does not double them.
    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            reads_submitted: self.reads_submitted + other.reads_submitted,
            reads_answered: self.reads_answered + other.reads_answered,
            reads_inline: self.reads_inline + other.reads_inline,
            reads_cached: self.reads_cached + other.reads_cached,
            reads_joined: self.reads_joined + other.reads_joined,
            writes: self.writes + other.writes,
            barriers: self.barriers + other.barriers,
            removes: self.removes + other.removes,
            renames: self.renames + other.renames,
            bytes_written: self.bytes_written + other.bytes_written,
            reads_refused: self.reads_refused + other.reads_refused,
            writes_refused: self.writes_refused + other.writes_refused,
            read_depth_peak: self.read_depth_peak.max(other.read_depth_peak),
            write_depth_peak: self.write_depth_peak.max(other.write_depth_peak),
            faults: self.faults + other.faults,
        }
    }
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
    /// Takes a block write. `false` means the queue is full and nothing was taken — backpressure rather
    /// than failure, so the caller keeps the block and offers it again.
    ///
    /// `creating` says this is the segment's first block, which is what brings the segment into being. One
    /// call rather than a create followed by a write: a segment's first block *is* its creation, and two
    /// statements that always happen together are two that can come apart (rule 16). The caller knows which
    /// it is from the blocks it has already put there, so a backend never pays a syscall to find out what
    /// its caller already knew.
    ///
    /// **Submitted rather than done, the way a read already is** (§20). What the caller must not assume is
    /// that returning means written: only the completion says that, and only a barrier says durable.
    fn submit_write(
        &mut self,
        handle: u64,
        object: ObjectId,
        offset: u64,
        block: &Block,
        creating: bool,
        now: u64,
    ) -> bool;
    /// Takes a barrier. Everything submitted before it is durable once its completion arrives.
    ///
    /// **One call with no segment, and that is a property of a filesystem rather than a simplification.**
    /// `fsync(fd)` makes a file's bytes durable — per file, which is what this does underneath — but a file
    /// that has just been created also needs its directory synced, or a crash can leave durable bytes in a
    /// file that does not exist. So durability is a fact about the store at a moment rather than a watermark
    /// per segment, which is the optimisation someone would otherwise reach for. What a barrier covered is
    /// the caller's to remember, because the caller is what submitted it.
    fn submit_barrier(&mut self, handle: u64, now: u64) -> bool;
    /// The next write or barrier that has finished, by the handle it was given. `None` while none has.
    fn poll_written(&mut self, now: u64) -> Option<(u64, Result<(), StoreFault>)>;
    /// Writes and barriers taken and not yet answered for.
    fn writes_inflight(&self) -> usize;
    /// Whether a write leaves the caller's thread. **A model in front of this store has to know**, because
    /// the two arrangements are different and its whole job is to describe one of them: a queued write
    /// makes a slow device show as a queue that fills, and an inline one makes it show as a thread that
    /// stops. Answered by the backing because the backing is what knows — the alternative is a model
    /// holding an assumption about a layer below it, which is how it came to price a lane that was there
    /// as though it were not.
    fn writes_are_queued(&self) -> bool {
        false
    }
    /// What this volume has done. **The one place that can answer for the disk rather than for a caller.**
    fn stats(&self) -> VolumeStats;
    /// `&mut` although reading changes nothing a caller can see: a store that models a device charges
    /// the read, and one that can fail counts it.
    fn read_at(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault>;
    /// False when the store will not take another read yet, which is backpressure rather than failure.
    fn submit(&mut self, handle: u64, object: ObjectId, offset: u64, now: u64) -> bool;
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
    /// Stops a segment existing. The one way the store shrinks: blocks are written once and never
    /// rewritten, so space comes back a whole day at a time, and only once nothing in the index points
    /// into that day.
    ///
    /// It answers nothing about how many blocks there were. The caller wrote them and so already knows,
    /// and a real store could not answer anyway — `unlink` does not count what it removes.
    ///
    /// **Submitted, on the same queue as the writes, because it is ordered against them.** A removal that
    /// overtook a write to the object it removes would leave the write in a file nothing will look at, and
    /// one that overtook a *read* would be the same race the other way — which held by unix semantics
    /// rather than by anything declared here (§20). One queue is what decides it instead of a coincidence.
    fn submit_remove(&mut self, handle: u64, object: ObjectId, now: u64) -> bool;
    /// Gives `from`'s bytes the name `to`, replacing whatever wore it, and makes the name itself durable.
    /// The one way an object is published: a reader that finds `to` finds all of it or the previous one,
    /// never a prefix (§19).
    ///
    /// On the queue for the same reason a removal is, and one more: the directory `fsync` inside it is a
    /// real one, and on the caller's thread it was an `fsync` on the thread that answers lookups.
    fn submit_rename(&mut self, handle: u64, from: ObjectId, to: ObjectId, now: u64) -> bool;
    /// Blocks this object spans from offset zero, which is where its last block ends. Zero for an object
    /// that is not there.
    ///
    /// **What a start-up asks instead of remembering.** Offsets are absolute (§16), so a segment's file
    /// ends where its last block does — the length *is* the high-water mark, and the restored index cannot
    /// give it: a block whose records all died leaves no slot to find it by. Asking the store is what lets
    /// the next block number be one that was never used, rather than one that overwrites bytes a previous
    /// life wrote.
    fn blocks_in(&mut self, object: ObjectId) -> u64;
    /// Whether the store has this object at all. Asked before a snapshot is read back, because a node that
    /// has never written one is the ordinary case and a device that refuses is not — and a read of block
    /// zero cannot tell them apart, both being `Missing`.
    fn exists(&mut self, object: ObjectId) -> bool;
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
    objects: [Option<SegmentFile>; OBJECT_VALUES],
    /// Submitted reads, answered in the order they were asked for and with no delay. A store that
    /// modelled a device would answer out of order; this one is the baseline that says what the
    /// structure does when the device is not the variable.
    submitted: VecDeque<(u64, ObjectId, u64)>,
    /// Writes and barriers, done as they are taken and answered in the order they were taken. Memory has
    /// no queue to be behind in, so the completion is immediate — which is what makes this the baseline a
    /// backend with a lane is measured against.
    written: VecDeque<(u64, Result<(), StoreFault>)>,
    stats: VolumeStats,
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
            objects: std::array::from_fn(|_| None),
            submitted: VecDeque::new(),
            written: VecDeque::new(),
            stats: VolumeStats::default(),
        }
    }
}

impl MemoryStore {
    fn block_at(&self, object: ObjectId, offset: u64) -> Result<&Block, StoreFault> {
        debug_assert!(
            offset.is_multiple_of(BLOCK_BYTES as u64),
            "an offset is a whole number of blocks, which is what direct IO requires of it"
        );
        self.objects[object.index()]
            .as_ref()
            .and_then(|file| file.at(offset))
            .ok_or(StoreFault::Missing)
    }
}

impl MemoryStore {
    /// The read itself. **Counting is the caller's**, because the same work is an inline read when
    /// `read_at` asks for it and the queue's when `poll` does, and a method that counted both would count
    /// one of them twice.
    fn read_block(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        match self.block_at(object, offset) {
            Ok(found) => {
                into.copy_from_slice(found);
                Ok(())
            }
            Err(fault) => {
                self.stats.faults += 1;
                Err(fault)
            }
        }
    }

    fn open_with(
        &mut self,
        object: ObjectId,
        offset: u64,
        block: &Block,
    ) -> Result<(), StoreFault> {
        // What `O_EXCL` is for, and a self-invariant rather than a fault: both sides of it are ours. A
        // segment brought into being twice would hold two days' blocks under one day's count, and a
        // snapshot's partial brought into being twice would be two dumps interleaved in one file.
        debug_assert!(
            self.objects[object.index()].is_none(),
            "an object is brought into being once, by its first block"
        );
        self.objects[object.index()] = Some(SegmentFile {
            base: offset,
            blocks: vec![Some(Block::copy_of(block))],
        });
        Ok(())
    }

    fn append(&mut self, object: ObjectId, offset: u64, block: &Block) -> Result<(), StoreFault> {
        let file = self.objects[object.index()]
            .as_mut()
            .ok_or(StoreFault::Missing)?;
        // At the end, or past it. Never before it: blocks are written once, so an offset already occupied is
        // the caller's block numbers and this sequence disagreeing, which is a self-invariant rather than the
        // store being broken. Past the end is a hole left by a write this store refused — the caller advances
        // its block number whether or not the write landed, because the records on the block it could not
        // write already hold addresses.
        debug_assert!(
            offset >= file.end(),
            "a block was written over one this object already has"
        );
        file.put(offset, block);
        Ok(())
    }
}

impl DurableStore for MemoryStore {
    fn submit_write(
        &mut self,
        handle: u64,
        object: ObjectId,
        offset: u64,
        block: &Block,
        creating: bool,
        _now: u64,
    ) -> bool {
        let done = if creating {
            self.open_with(object, offset, block)
        } else {
            self.append(object, offset, block)
        };
        self.stats.took_write(block.len(), self.written.len() + 1);
        self.stats.answered_write(done.is_ok());
        self.written.push_back((handle, done));
        true
    }

    /// Nothing to do, and nothing dishonest about that: memory has no second layer to push bytes into.
    /// Answered anyway, so the caller's barrier bookkeeping runs here exactly as it does over a device.
    fn submit_barrier(&mut self, handle: u64, _now: u64) -> bool {
        self.stats.took_barrier(self.written.len() + 1);
        self.written.push_back((handle, Ok(())));
        true
    }

    fn poll_written(&mut self, _now: u64) -> Option<(u64, Result<(), StoreFault>)> {
        self.written.pop_front()
    }

    fn writes_inflight(&self) -> usize {
        self.written.len()
    }

    fn read_at(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        self.stats.reads_inline += 1;
        self.read_block(object, offset, into)
    }

    fn submit(&mut self, handle: u64, object: ObjectId, offset: u64, _now: u64) -> bool {
        self.stats.took_read(self.submitted.len() + 1);
        self.submitted.push_back((handle, object, offset));
        true
    }

    fn poll(&mut self, _now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        let (handle, object, offset) = self.submitted.pop_front()?;
        let answered = self.read_block(object, offset, into).map(|()| handle);
        self.stats.answered_read(answered.is_ok());
        Some(answered)
    }

    fn inflight(&self) -> usize {
        self.submitted.len()
    }

    fn submit_remove(&mut self, handle: u64, object: ObjectId, _now: u64) -> bool {
        self.stats.removes += 1;
        // One drop, which is what `unlink` costs. What this replaced went looking through a map for the
        // blocks of a day, and that was a stand-in's cost rather than a store's: the sweep bench had to
        // leave the round that frees a day out of its numbers because it was the worst round at every
        // size and hid the one being measured.
        self.objects[object.index()] = None;
        self.written.push_back((handle, Ok(())));
        true
    }

    /// A move, which is what a rename is: the bytes do not go anywhere and whatever wore the name is
    /// dropped by being replaced.
    fn submit_rename(&mut self, handle: u64, from: ObjectId, to: ObjectId, _now: u64) -> bool {
        self.stats.renames += 1;
        let done = match self.objects[from.index()].take() {
            Some(moved) => {
                self.objects[to.index()] = Some(moved);
                Ok(())
            }
            None => Err(StoreFault::Missing),
        };
        self.written.push_back((handle, done));
        true
    }

    fn blocks_in(&mut self, object: ObjectId) -> u64 {
        self.objects[object.index()]
            .as_ref()
            .map(|file| file.end() / BLOCK_BYTES as u64)
            .unwrap_or(0)
    }

    fn exists(&mut self, object: ObjectId) -> bool {
        self.objects[object.index()].is_some()
    }

    fn stats(&self) -> VolumeStats {
        self.stats
    }
}

/// The one segment that is on no disk: an address in it is a record still in the writeback buffer,
/// waiting to be carried on. Segments are days and thirty-four are ever live, so the top of the six-bit
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
/// A block that is closed and not yet on a device: its bytes will not change again, and where it goes is
/// already decided. What sits between the two halves `seal_block` used to do in one call (§20).
///
/// **It carries no bytes.** The block itself is already in residency, because closing is what puts it there
/// — see `seal_block`. This is the note that it still has to be written.
#[derive(Clone, Copy)]
struct Unwritten {
    block: u64,
    segment: u8,
    /// Whether this is the segment's first block, and so what brings the segment into being.
    opening: bool,
}

/// What the log owes the volume, in the order it owes it.
///
/// **One backlog rather than two, because the order between them is the point.** A day's file is removed
/// when nothing points into it any more, and the blocks written to that day are ahead of the removal in
/// this queue — so the removal cannot overtake them. Two deques would have made that order a property of
/// which one the round drained first (rule 18).
#[derive(Clone, Copy)]
enum Owed {
    Block(Unwritten),
    /// A day's file, once the index has no entry in it.
    Free(u8),
}

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
    ///
    /// Reads only, and that is the point of there being two: the depth a read side wants is Little's law
    /// on the read rate — reads a second times the latency of one — and the depth a write side wants is
    /// the block seal rate against one ordered thread. Two arithmetics, and one number could only be
    /// right for one of them.
    pub queue_depth: usize,
    /// Writes and barriers the lane will hold at once. Past this the caller keeps the block and offers it
    /// again, which is how a device slower than the ledger becomes backpressure rather than memory.
    pub write_queue_depth: usize,
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
    /// One file per segment, in a directory that has been opened, with `read_threads` threads issuing the
    /// `pread`s. Zero reads synchronously, which is the baseline the pool is measured against.
    Files {
        dir: std::fs::File,
        path: PathBuf,
        read_threads: usize,
        /// Whether `pwrite` and `fsync` go to a thread of their own. False is the synchronous baseline
        /// every number is compared against, and what a virtual clock can run (§20).
        write_lane: bool,
    },
}

impl OpenBacking {
    /// The directory to put a segment's files in, opened and created if it was not there.
    ///
    /// `read_threads` follows Little's law on the *store read* rate — the share of lookups that miss both
    /// memory windows — and not on the lookup rate: threads ≈ reads a second × the latency of one. The
    /// design's sixteen comes from 0.5ms against tens of thousands a second. A configuration that forces every
    /// read to miss needs an order more, and sixteen failing to keep up there is the arithmetic holding rather
    /// than the pool being wrong.
    pub fn files(path: &Path, read_threads: usize, write_lane: bool) -> Result<Self, LedgerError> {
        let (dir, path) =
            crate::files::open_directory(path).map_err(|_| LedgerError::ConfigInvalid)?;
        Ok(Self::Files {
            dir,
            path,
            read_threads,
            write_lane,
        })
    }

    /// Whether two backings are the same disk, and so want one store between them (§20).
    ///
    /// **It answers only the case it cannot be wrong about.** The same directory is the same volume by
    /// definition — `open_directory` canonicalises, so two spellings of one path are one answer here.
    /// Two *different* directories on one disk is the case that matters and the case nothing can detect:
    /// `st_dev` is wrong in both directions (two partitions of one NVMe have different ids and one queue;
    /// LVM, RAID and network volumes have one id across several devices), so it takes a declaration, and
    /// that declaration does not exist yet. Memory is one volume because there is one memory.
    pub fn same_volume(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Memory, Self::Memory) => true,
            (Self::Files { path: ours, .. }, Self::Files { path: theirs, .. }) => ours == theirs,
            _ => false,
        }
    }

    /// The store this backing is, with no device modelled in front of it.
    pub fn open(self, depths: QueueDepths) -> Box<dyn DurableStore> {
        match self {
            Self::Memory => Box::new(MemoryStore::default()),
            Self::Files {
                dir,
                path,
                read_threads,
                write_lane,
            } => Box::new(crate::files::FileStore::new(
                dir,
                path,
                depths,
                read_threads,
                write_lane,
            )),
        }
    }
}

/// How deep each of a volume's two queues may go. Grouped because they are one property of one device
/// asked in two different arithmetics (rule 11), and passed together so a caller cannot set one and
/// forget the other.
#[derive(Debug, Clone, Copy)]
pub struct QueueDepths {
    pub read: usize,
    pub write: usize,
}

impl StoreModel {
    /// The two depths, each at least one.
    pub fn depths(&self) -> QueueDepths {
        QueueDepths {
            read: self.queue_depth.max(1),
            write: self.write_queue_depth.max(1),
        }
    }

    pub fn build(&self, backing: OpenBacking, seed: u64) -> Box<dyn DurableStore> {
        let exact = backing.open(self.depths());
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
    /// Reads handed to the store below and not yet answered by it: handle, object, offset, and when this
    /// model says the device is done with them.
    inflight: Vec<(u64, ObjectId, u64, u64)>,
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
    write_queue_depth: usize,
    /// Device time charged by synchronous calls and not yet handed to whoever has a clock.
    charged_nanos: u64,
    /// Writes and barriers this model refused, waiting to be answered as failed completions. A queue rather
    /// than a flag because expiry is not the only thing that can have several in flight, and a dropped one
    /// would be a block the caller waits on for ever.
    refused: VecDeque<u64>,
    /// Queued writes and barriers, in submission order, with when this model says the lane is done with
    /// each. Empty unless the backing queues its writes.
    write_due: VecDeque<(u64, u64)>,
    /// Answers the backing has given and this model has not released, because its own time for them has
    /// not passed.
    write_answered: VecDeque<(u64, Result<(), StoreFault>)>,
    /// When the modelled lane next falls idle. One server: a write waits for the one in front of it.
    write_busy_until: u64,
    fault_every: u32,
    corrupt_every: u32,
    calls: u64,
    reads: u64,
    /// What this model added that the backing never saw: the calls it refused for a full queue and the
    /// faults it invented. The backing counts what reached it, and these two are what did not.
    invented: VolumeStats,
}

impl LatencyStore {
    pub fn new(inner: Box<dyn DurableStore>, model: StoreModel, seed: u64) -> Self {
        let queue_depth = model.queue_depth.max(1);
        let write_queue_depth = model.write_queue_depth.max(1);
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
            write_queue_depth,
            charged_nanos: 0,
            refused: VecDeque::new(),
            write_due: VecDeque::new(),
            write_answered: VecDeque::new(),
            write_busy_until: 0,
            fault_every: model.fault_every,
            corrupt_every: model.corrupt_every,
            calls: 0,
            reads: 0,
            invented: VolumeStats::default(),
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

    fn writes_queued(&self) -> usize {
        self.write_due.len()
    }

    /// A namespace change, in its turn and at no cost. One shape for both because they differ only in
    /// which call the backing makes.
    fn queue_namespace(
        &mut self,
        handle: u64,
        now: u64,
        submit: impl FnOnce(&mut dyn DurableStore, u64, u64) -> bool,
    ) -> bool {
        if !self.inner.writes_are_queued() {
            if self.refuses() {
                self.refused.push_back(handle);
                return true;
            }
            return submit(self.inner.as_mut(), handle, now);
        }
        if self.writes_queued() >= self.write_queue_depth {
            self.invented.writes_refused += 1;
            return false;
        }
        if self.refuses() {
            self.invented.faults += 1;
            self.refused.push_back(handle);
            return true;
        }
        if !submit(self.inner.as_mut(), handle, now) {
            return false;
        }
        let due = self.serve_write(now, Cost::default());
        self.write_due.push_back((handle, due));
        true
    }

    /// When the lane will be done with this one. **One server, not many**, because that is what a write
    /// lane is: writes do not commute, so they are served in order by one thread, and a second write
    /// waits for the first rather than overlapping it. The read side draws from a rate gate instead,
    /// because reads do overlap.
    fn serve_write(&mut self, now: u64, cost: Cost) -> u64 {
        let start = now.max(self.write_busy_until);
        let due = start + cost.draw(&mut self.prng);
        self.write_busy_until = due;
        due
    }
}

impl DurableStore for LatencyStore {
    /// **Charged where the backing puts it, which is the one thing this had wrong.** A write on a lane is a
    /// queue's cost: it is admitted, the queue serves it, and the caller goes on — so it gets a deadline,
    /// exactly as a read does. A write the backing does inline is the caller's thread stopping, so it is
    /// charged to `busy_until`. Both arrangements exist and the backing is what knows which
    /// (`writes_are_queued`); before, this assumed the second and priced a lane as though it were not
    /// there, which measured the model rather than the arrangement.
    fn submit_write(
        &mut self,
        handle: u64,
        object: ObjectId,
        offset: u64,
        block: &Block,
        creating: bool,
        now: u64,
    ) -> bool {
        if !self.inner.writes_are_queued() {
            self.charge(self.write);
            if self.refuses() {
                self.refused.push_back(handle);
                return true;
            }
            return self
                .inner
                .submit_write(handle, object, offset, block, creating, now);
        }
        if self.writes_queued() >= self.write_queue_depth {
            self.invented.writes_refused += 1;
            return false;
        }
        if self.refuses() {
            self.invented.faults += 1;
            self.refused.push_back(handle);
            return true;
        }
        if !self
            .inner
            .submit_write(handle, object, offset, block, creating, now)
        {
            return false;
        }
        let due = self.serve_write(now, self.write);
        self.write_due.push_back((handle, due));
        true
    }

    fn submit_barrier(&mut self, handle: u64, now: u64) -> bool {
        if !self.inner.writes_are_queued() {
            self.charge(self.sync);
            if self.refuses() {
                self.refused.push_back(handle);
                return true;
            }
            return self.inner.submit_barrier(handle, now);
        }
        if self.writes_queued() >= self.write_queue_depth {
            self.invented.writes_refused += 1;
            return false;
        }
        if self.refuses() {
            self.invented.faults += 1;
            self.refused.push_back(handle);
            return true;
        }
        if !self.inner.submit_barrier(handle, now) {
            return false;
        }
        let due = self.serve_write(now, self.sync);
        self.write_due.push_back((handle, due));
        true
    }

    /// A refusal this model invented is answered here rather than at submit: the caller is told through the
    /// completion, which is the one path a real device's failure takes too (rule 17 — nothing observable
    /// changes at submit but the promise to answer).
    ///
    /// A queued write leaves when the backing has answered it **and** this model's time for it has passed,
    /// which is the same floor a read's completion is — a device modelled slower than the one underneath
    /// dominates, and one modelled faster cannot be expressed.
    fn poll_written(&mut self, now: u64) -> Option<(u64, Result<(), StoreFault>)> {
        if let Some(handle) = self.refused.pop_front() {
            return Some((handle, Err(StoreFault::Device)));
        }
        if !self.inner.writes_are_queued() {
            return self.inner.poll_written(now);
        }
        while let Some(answered) = self.inner.poll_written(now) {
            self.write_answered.push_back(answered);
        }
        // In submission order, because one lane serves them in that order and the caller's barrier
        // bookkeeping rests on it.
        let (handle, due) = *self.write_due.front()?;
        if due > now {
            return None;
        }
        let at = self
            .write_answered
            .iter()
            .position(|(asked, _)| *asked == handle)?;
        let (_, outcome) = self.write_answered.remove(at)?;
        self.write_due.pop_front();
        Some((handle, outcome))
    }

    fn writes_inflight(&self) -> usize {
        self.refused.len() + self.write_due.len() + self.inner.writes_inflight()
    }

    fn writes_are_queued(&self) -> bool {
        self.inner.writes_are_queued()
    }

    /// The backing's, plus what this model refused or failed that never reached it.
    fn stats(&self) -> VolumeStats {
        self.inner.stats().merge(self.invented)
    }

    /// The read that cannot be submitted and harvested: the apply path is in order and cannot park a
    /// decision half way, and the expiry walk reads a whole block at a time. Both hold the thread, so both
    /// are charged to it rather than to the device's queue.
    fn read_at(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        self.charge(self.read);
        if self.refuses() {
            return Err(StoreFault::Device);
        }
        self.inner.read_at(object, offset, into)?;
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
    fn submit(&mut self, handle: u64, object: ObjectId, offset: u64, now: u64) -> bool {
        if self.inflight.len() + self.completed.len() >= self.queue_depth {
            self.invented.reads_refused += 1;
            return false;
        }
        // The store below first: a deadline recorded for a read it would not take is a read this would
        // release having never done it (rule 17).
        if !self.inner.submit(handle, object, offset, now) {
            return false;
        }
        let due = self.device.serve(now, &mut self.prng);
        self.inflight.push((handle, object, offset, due));
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

    fn take_charge(&mut self) -> u64 {
        std::mem::take(&mut self.charged_nanos)
    }

    /// Freeing costs the device nothing this model charges for: it is off any request's path, and a device
    /// that made it expensive would be one whose extents this store does not model. **It still takes its
    /// turn in the queue**, at no cost, because leaving it out would let it overtake the writes it is
    /// ordered against — which is the whole reason it is on this queue.
    fn submit_remove(&mut self, handle: u64, object: ObjectId, now: u64) -> bool {
        self.queue_namespace(handle, now, |inner, handle, now| {
            inner.submit_remove(handle, object, now)
        })
    }

    /// A namespace change costs the device nothing this model charges for, for the same reason freeing does
    /// not. What it does cost — the directory sync a real backing does inside it — is a barrier's cost, and
    /// a barrier is priced beside it.
    fn submit_rename(&mut self, handle: u64, from: ObjectId, to: ObjectId, now: u64) -> bool {
        self.queue_namespace(handle, now, |inner, handle, now| {
            inner.submit_rename(handle, from, to, now)
        })
    }

    fn blocks_in(&mut self, object: ObjectId) -> u64 {
        self.inner.blocks_in(object)
    }

    fn exists(&mut self, object: ObjectId) -> bool {
        self.inner.exists(object)
    }
}

/// The records: a writeback buffer of recent blocks, the blocks carried out of it that are still worth
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
/// Residency is what keeps IO off the resolutions that come soon after — a block already carried on stays readable
/// in memory long after its content is durable. Written and resident are independent states, and holding
/// them apart is what lets the first window be an hour while the second is a day. Residency costs far
/// less than a day of arrivals, because what is resident has already been compacted: the survivors.
pub struct RecordLog {
    store: Box<dyn DurableStore>,
    /// Recent blocks, oldest first; the last is the one being filled. Not on the store yet.
    buffer: VecDeque<Filling>,
    /// Ordinal of `buffer.front()`, so an address stays unique after blocks are carried away.
    oldest: u64,
    /// Blocks the buffer may hold before its oldest is compacted out — the flush window. A count, not a
    /// duration: the engine has no clock, so a window in time is this divided by a rate.
    flush_blocks: usize,
    /// Survivors accumulate here so a store block is packed rather than one per compaction. Written out
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
    /// Blocks closed and not yet written. Bounded by what one drain round can close, because the same round
    /// issues them — it is a hand-off inside a round rather than a backlog, and it becomes a real queue only
    /// when the write leaves this thread (§20).
    ///
    /// They are read from as well as written from: a record on a closed block is still in memory, and a
    /// lookup that could not see it would go to a device that has not been given the block yet.
    pending_writes: VecDeque<Owed>,
    /// Writes the store has taken and not yet answered for, by the handle it was given. Only the note
    /// travels: the block itself is in residency the whole time.
    ///
    /// **What this gates is eviction, not reading.** A block whose write is outstanding may not leave
    /// residency, or a read of it would go to a device that has not been given it — which is the invariant
    /// the whole read path rests on: *a block that is not in the memory tier has already been written*
    /// (rule 22).
    submitted_writes: VecDeque<(u64, Owed)>,
    /// The barrier in flight, if any. **One at a time on purpose**: a barrier covers everything submitted
    /// before it, so a second while one is outstanding covers a subset of the first and buys nothing.
    barrier: Option<u64>,
    /// Blocks closed *after* the barrier in flight was submitted, which it therefore does not cover. When
    /// the barrier completes this becomes `unsynced`; when it fails, `unsynced` already covers them because
    /// the two runs are contiguous.
    after_barrier: Option<(u64, ApplyIndex)>,
    /// Sequence numbers for writes and barriers. One counter, because the completion queue is one queue;
    /// the handle it becomes carries `IoOwner::Blocks`, so a store shared with the snapshot can tell the
    /// two apart.
    write_handles: u64,
    /// Completions this log did not ask for, waiting for the owner that did. Non-empty only on a volume two
    /// callers share, and bounded by that caller's own queue — it cannot have more outstanding than the
    /// store will hold.
    foreign: VecDeque<(u64, Result<(), StoreFault>)>,
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
    /// The expiry sweep's block reads, by handle: which day's block each is for.
    ///
    /// **The sweep submits like everything else now.** It used to read inline, on the thread that answers
    /// lookups, and that was the last read here that did so without a reason — the apply path's fallback
    /// has one (it is in order and cannot park a decision half way) and this had none beyond being older
    /// than the queue. Synchronous IO is the exception, not the shape.
    sweeping_blocks: FxHashMap<u64, (u8, u64)>,
    sweep_handles: u64,
    appended: u64,
    died_in_buffer: u64,
    carried_on: u64,
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
            pending_writes: VecDeque::new(),
            submitted_writes: VecDeque::new(),
            barrier: None,
            after_barrier: None,
            write_handles: 0,
            foreign: VecDeque::new(),
            segment: 0,
            days: [BlockRange::default(); SEGMENT_VALUES],
            next_block: 0,
            unsynced: None,
            scratch: Block::zeroed(),
            fetching: FxHashMap::default(),
            sweeping_blocks: FxHashMap::default(),
            sweep_handles: 0,
            appended: 0,
            died_in_buffer: 0,
            carried_on: 0,
            freed: 0,
            faults: 0,
            corruptions: 0,
            fault_owed: false,
            left_memory: 0,
            buffer_reads: 0,
            resident_reads: 0,
            store_reads: 0,
            apply_store_reads: 0,
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

    /// Blocks the buffer is holding, the one being filled included. Against the window it was sized for,
    /// this is how far behind the drain has fallen.
    pub fn buffered_blocks(&self) -> usize {
        self.buffer.len()
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
    /// **Queued rather than done here**, behind whatever this log still owes that day. The removal is
    /// offered to the volume in `submit_writes` like every other thing this log owes it, and a volume that
    /// will not take it yet keeps it in the queue rather than losing it — which a call made and dropped
    /// here would have done, because the day's range is reset now and `reclaim` would never ask again.
    pub fn free_segment(&mut self, segment: u8) -> usize {
        let freed = self.days[segment as usize].blocks as usize;
        self.pending_writes.push_back(Owed::Free(segment));
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
    pub fn sync(&mut self, now: u64) -> bool {
        // Everything closed has to be *submitted* before the barrier that claims to cover it. One call
        // rather than two statements a caller could get the wrong way round (rule 16): a barrier that
        // overtook a write it was meant to cover would make coverage claim a block a restart cannot read.
        // The lane keeps the order from there on, which is why the write side is a queue and not a pool.
        self.submit_writes(now);
        if self.unsynced.is_none() || self.barrier.is_some() {
            return false;
        }
        // A barrier is not taken until the store says so, and nothing is recorded before it does (rule 17):
        // a handle spent on a barrier the queue refused would be one nothing ever completes, and coverage
        // would stop for ever waiting for it.
        let handle = IoOwner::Blocks.handle(self.write_handles + 1);
        if !self.store.submit_barrier(handle, now) {
            return false;
        }
        self.write_handles += 1;
        self.barrier = Some(handle);
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
    /// One store read for the whole block, and residency is not tried first: a day being expired is
    /// `retention + grace` days old and a configuration is refused unless residency is shorter than that, so
    /// its blocks left that window long ago — the same reason `free_segment` does not touch residency.
    ///
    /// **A block that is closed and not yet written is tried, and that is not the same argument.** Residency
    /// is a window a validated configuration bounds; `pending_writes` is this instant's, and nothing about a
    /// day's age says a block cannot be sitting in it. It cannot happen in the worker's round as the round
    /// is ordered today — the sweep runs before the drain, so anything closed earlier is already issued —
    /// and that is precisely the kind of invariant rule 18 says not to leave resting on an ordering. The
    /// queue holds at most one round's closes and is normally empty here, so the scan costs nothing.
    ///
    /// A closure rather than an iterator because the block is decoded out of the log's own scratch buffer,
    /// so nothing is allocated per record or per block. The address goes with each record because that is
    /// what the caller checks the index against — a record is alive exactly when the index still points at
    /// it, which is the same test compaction makes.
    pub fn walk_day_block(
        &mut self,
        segment: u8,
        at: u64,
        now: u64,
        visit: &mut dyn FnMut(TxId, HoldData, RecordAddr),
    ) -> Walked {
        let Some(block) = self.days[segment as usize].block_at(at) else {
            return Walked::End;
        };
        if let Some(held) = block
            .checked_sub(self.oldest_resident)
            .and_then(|slot| self.resident.get(slot as usize))
        {
            for index in 0..held.filled {
                let addr = RecordAddr::new(segment, block, index as u8);
                if let Some((key, hold)) = held.get(index, addr) {
                    visit(key, hold, addr);
                }
            }
            return Walked::Visited;
        }
        // **Submitted like every other read.** It used to happen right here, on the thread that answers
        // lookups, and it was the last read in this file doing so without a reason: the apply path's
        // fallback has one — it is in order and cannot park a decision half way — and this had none beyond
        // being older than the queue. Synchronous IO is the exception, not the shape.
        let handle = IoOwner::Sweep.handle(self.sweep_handles + 1);
        if !self.store.submit(
            handle,
            ObjectId::segment(segment),
            block * BLOCK_BYTES as u64,
            now,
        ) {
            return Walked::Busy;
        }
        self.sweep_handles += 1;
        self.sweeping_blocks.insert(handle, (segment, block));
        Walked::Asked
    }

    /// Every record on the block the last completion brought back, for the sweep that asked for it.
    fn visit_scratch(
        &self,
        segment: u8,
        block: u64,
        visit: &mut dyn FnMut(TxId, HoldData, RecordAddr),
    ) {
        for index in 0..RECORDS_PER_BLOCK {
            let addr = RecordAddr::new(segment, block, index as u8);
            let from = index * RECORD_BYTES;
            let (key, hold) = decode(&self.scratch[from..from + RECORD_BYTES], addr);
            if key != TxId::ABSENT {
                visit(key, hold, addr);
            }
        }
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
            self.seal_block();
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
            self.seal_block();
        }
        let index = self.store_open.put_at(key, hold, from);
        self.carried_on += 1;
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
    /// block carried on and still inside the residency window. `None` means only the store has it.
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
            .read_at(
                ObjectId::segment(addr.segment()),
                addr.block_offset(),
                &mut self.scratch,
            )
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
        if !self.store.submit(
            handle,
            ObjectId::segment(addr.segment()),
            addr.block_offset(),
            now,
        ) {
            return false;
        }
        self.fetching.insert(handle, addr);
        true
    }

    /// The next fetch finished by `now`. Completions may arrive in an order the reads were not asked
    /// in, which is what the orderer exists for.
    pub fn harvest(
        &mut self,
        now: u64,
        sweep: &mut dyn FnMut(TxId, HoldData, RecordAddr),
    ) -> Option<(u64, RecordAddr, TxId, HoldData)> {
        let completed = self.store.poll(now, &mut self.scratch)?;
        let Ok(handle) = completed else {
            // The lookup that asked will never be answered, and that is the honest outcome rather than a
            // gap: its lane stalls, the seal below stops anything more being applied, and a drain that
            // never completes is what says to replace this leader (rule 19).
            self.note_fault();
            self.sweeping_blocks.clear();
            return None;
        };
        if !self.scratch_intact() {
            self.sweeping_blocks.remove(&handle);
            return None;
        }
        self.store_reads += 1;
        // The sweep's, and it is told apart by the tag rather than by failing to find it among the
        // lookups: one queue answers both, so a completion has to say whose it is (rule 18).
        if IoOwner::Sweep.owns(handle) {
            if let Some((segment, block)) = self.sweeping_blocks.remove(&handle) {
                self.visit_scratch(segment, block, sweep);
            }
            return None;
        }
        let addr = self.fetching.remove(&handle)?;
        let at = addr.index() as usize * RECORD_BYTES;
        let (key, hold) = decode(&self.scratch[at..at + RECORD_BYTES], addr);
        Some((handle, addr, key, hold))
    }

    /// Blocks the sweep has asked for and not been given yet.
    pub fn sweeping_blocks(&self) -> usize {
        self.sweeping_blocks.len()
    }

    /// Records appended, records that never left the buffer because they were resolved first, records
    /// carried on to the store, and reads answered from memory against reads that went to the store.
    /// The second is the number the design's capacity rests on and the one its own inputs disagree
    /// about.
    pub fn traffic(&self) -> LogTraffic {
        LogTraffic {
            appended: self.appended,
            died_in_buffer: self.died_in_buffer,
            carried_on: self.carried_on,
            left_memory: self.left_memory,
            buffer_reads: self.buffer_reads,
            resident_reads: self.resident_reads,
            store_reads: self.store_reads,
            apply_store_reads: self.apply_store_reads,
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
    /// **Closes the block. It does not write it**, and that split is why this exists apart from
    /// `submit_writes` below (design notes §20).
    ///
    /// Closing is what a checksum needs — this is the one moment the bytes stop changing — and writing is
    /// what a device does. They were one call, which is why the write could not be moved anywhere: there was
    /// no seam to hold the first half and hand off the second. A closed block now waits in `pending_writes`
    /// until something issues it, and today that something is still this thread.
    ///
    /// **Coverage is deliberately unaffected.** `unsynced` is recorded here, at the close, so a block that is
    /// closed and not yet written is already outside what a snapshot may carry — which was true before and is
    /// now true for one more reason.
    fn seal_block(&mut self) {
        // Stamped here and nowhere else: this is the one moment a block's bytes stop changing, which is what
        // makes a whole-block checksum possible at all.
        self.store_open.bytes.stamp();
        // A segment's first block is what brings it into being, and a later one lands in a segment that
        // exists. This side knows which from the day's own count, so the store is told rather than asked —
        // a real one would otherwise pay a syscall to find out what its caller already knew.
        let opening = self.days[self.segment as usize].blocks == 0;
        // The oldest block a sync has not covered, and the position it began at. Only the first seal since a
        // sync records it: later ones are newer, and it is the oldest that bounds both coverage and which
        // slots a snapshot may keep.
        // Which of the two runs this block joins is decided by whether a barrier is outstanding. A block
        // closed after one was submitted is not covered by it, and folding it into `unsynced` would make the
        // barrier's completion claim a block the device was never asked about — a snapshot would then carry
        // slots naming a block a restart cannot read, which is the one failure §15's whole boundary exists
        // to prevent.
        if self.barrier.is_some() {
            self.after_barrier
                .get_or_insert((self.next_block, self.store_open.began_at));
        } else {
            self.unsynced
                .get_or_insert((self.next_block, self.store_open.began_at));
        }
        self.days[self.segment as usize].note(self.next_block);
        let fresh = self.spare.pop().unwrap_or_else(Filling::new);
        let closed = std::mem::replace(&mut self.store_open, fresh);
        // **Residency takes it now, not when the device answers.** A block that is not in the memory tier
        // has already been written — that is what lets a read that misses here go straight to the device
        // without asking anything else, and it is only true if closing is what puts a block in. Filling
        // residency on completion instead left a block closed, unwritten and unresident all at once, and
        // every reader had to be taught about the gap one at a time (rule 22, design notes §20).
        //
        // It also puts residency back in block order by construction — pushed at the back as blocks close,
        // dropped from the front — which is the same shape the writeback buffer has and depends on nothing
        // outside this file.
        self.resident.push_back(closed);
        self.pending_writes.push_back(Owed::Block(Unwritten {
            block: self.next_block,
            segment: self.segment,
            opening,
        }));
        self.next_block += 1;
        self.store_open.filled = 0;
    }

    /// Issues every closed block that is waiting, oldest first, and answers whether there were any.
    ///
    /// **In order, because writes to one segment are not interchangeable**: a segment's first block is what
    /// brings it into being, so it has to reach the device before the ones after it. A queue keeps that for
    /// free while one thread serves it — which is why §20 has the write side as an ordered lane and not a
    /// pool.
    ///
    /// Residency is filled from here rather than at the close, so a block enters that window once a device
    /// has actually been given it. It is trimmed here for the reason it always was: this is the only event
    /// that adds to it.
    pub fn submit_writes(&mut self, now: u64) -> bool {
        let mut any = false;
        let Self {
            store,
            resident,
            oldest_resident,
            pending_writes,
            submitted_writes,
            write_handles,
            ..
        } = self;
        while let Some(owed) = pending_writes.front().copied() {
            let handle = IoOwner::Blocks.handle(*write_handles + 1);
            // Refused means the store's queue is full: the note stays where it is and is offered again next
            // round. Nothing observable moves before the store has taken it (rule 17) — the handle is only
            // spent once the submit succeeded.
            let taken = match owed {
                Owed::Block(unwritten) => {
                    let offset =
                        RecordAddr::new(unwritten.segment, unwritten.block, 0).block_offset();
                    // The bytes are in residency, because closing put them there. A block with no entry
                    // there is one eviction has taken, which cannot happen before its write is answered for.
                    let Some(held) = unwritten
                        .block
                        .checked_sub(*oldest_resident)
                        .and_then(|slot| resident.get(slot as usize))
                    else {
                        break;
                    };
                    store.submit_write(
                        handle,
                        ObjectId::segment(unwritten.segment),
                        offset,
                        &held.bytes,
                        unwritten.opening,
                        now,
                    )
                }
                Owed::Free(segment) => store.submit_remove(handle, ObjectId::segment(segment), now),
            };
            if !taken {
                break;
            }
            *write_handles += 1;
            pending_writes.pop_front();
            submitted_writes.push_back((handle, owed));
            any = true;
        }
        any
    }

    /// Puts this log back where a previous life left it, from the addresses a restored index still points
    /// at and from the volume itself. Answers the segments that have a file and nothing alive in them.
    ///
    /// **Two sources and neither could do it alone.** The slots say which blocks still matter, so a day's
    /// range is the span they cover — a block below or above that span holds only dead records, which is
    /// the same condition `reclaim` already uses on a whole day. What the slots cannot say is how far the
    /// blocks *went*: a block whose records all died leaves nothing to find it by, and writing the next
    /// one at its number would put two records at one address. The volume answers that, because offsets
    /// are absolute and a file therefore ends where its last block does (§16).
    ///
    /// The segments it answers with are the leak this would otherwise leave: a day with a file and no live
    /// slot is never reclaimed, because `reclaim` skips a day whose range is empty and a restored range is
    /// empty exactly when nothing points into it.
    /// The block number the next seal will take. What a restart has to derive, and what nothing but the
    /// volume can tell it: a block whose records all died leaves no slot to find it by.
    pub fn next_block(&self) -> u64 {
        self.next_block
    }

    pub fn reconcile(&mut self, live: &[(u8, u64, u64)]) -> Vec<u8> {
        let mut orphans = Vec::new();
        for segment in 0..SEGMENT_VALUES {
            let blocks = self.store.blocks_in(ObjectId::segment(segment as u8));
            self.next_block = self.next_block.max(blocks);
            let span = live.iter().find(|(at, ..)| *at as usize == segment);
            match span {
                Some(&(_, first, last)) => {
                    self.days[segment] = BlockRange {
                        first,
                        blocks: last - first + 1,
                    };
                }
                None if blocks > 0 => orphans.push(segment as u8),
                None => {}
            }
        }
        // Nothing is resident and nothing is buffered: what a restart has is what the volume has.
        self.oldest_resident = self.next_block;
        orphans
    }

    /// The volume these blocks are on, for whoever else is on it.
    ///
    /// **Not a way past the log, and the namespace is what keeps it honest**: an object is a day or a
    /// snapshot, and the day ↔ segment mapping stays here. What a sharer gets is the disk, which is the
    /// point of there being one — every IO into it is submitted, counted and queued at this one place
    /// whoever asked for it (§20).
    pub fn volume(&mut self) -> &mut dyn DurableStore {
        self.store.as_mut()
    }

    /// What the volume these blocks are on has done. Counted by the volume rather than by this log,
    /// which is what makes it an answer about the disk rather than about one of its callers.
    pub fn volume_stats(&self) -> VolumeStats {
        self.store.stats()
    }

    /// The next completion this log polled that belongs to somebody else. Drained by that owner, which is
    /// how a shared volume keeps one poller and still answers two callers.
    pub fn take_foreign(&mut self) -> Option<(u64, Result<(), StoreFault>)> {
        self.foreign.pop_front()
    }

    /// Blocks closed or submitted and not yet answered for.
    pub fn writes_outstanding(&self) -> usize {
        self.pending_writes.len() + self.submitted_writes.len()
    }

    /// Takes every write and barrier the store has answered. Answers whether it took any.
    ///
    /// **A write's completion is what puts its block into residency**, so a block enters that window once a
    /// device has actually been given it. Completions arrive in submission order because the write side is an
    /// ordered lane (§20), which is what keeps residency's block numbers contiguous — the one property
    /// `oldest_resident` rests on.
    pub fn collect_writes(&mut self, now: u64) -> bool {
        let mut any = false;
        while let Some((handle, outcome)) = self.store.poll_written(now) {
            // Somebody else's, on a store this volume shares. One queue has one poller, so what does not
            // belong to the blocks is put where its owner will find it rather than dropped — dropping it
            // would leave that owner waiting on a completion that already came (rule 18: the handle says
            // whose it is, and nobody has to infer it).
            if !IoOwner::Blocks.owns(handle) {
                self.foreign.push_back((handle, outcome));
                any = true;
                continue;
            }
            any = true;
            if self.barrier == Some(handle) {
                self.barrier = None;
                match outcome {
                    // Everything submitted before it is durable, so what is left unsynced is exactly what
                    // was closed after it went out.
                    Ok(()) => self.unsynced = self.after_barrier.take(),
                    // It covered nothing. The two runs are contiguous and `unsynced` is the older, so the
                    // later one folds into it — and if there was no older one, it becomes it.
                    Err(_) => {
                        let later = self.after_barrier.take();
                        if self.unsynced.is_none() {
                            self.unsynced = later;
                        }
                        self.note_fault();
                    }
                }
                continue;
            }
            let Some(at) = self
                .submitted_writes
                .iter()
                .position(|(asked, _)| *asked == handle)
            else {
                continue;
            };
            self.submitted_writes.remove(at);
            // Nothing here can repair a store that would not take a block, and nothing here decides what to
            // do about it: the index already points at every record on it, so they are unreachable and the
            // log says they exist. The reaction is rule 19's seal, latched and handed over a round later.
            if outcome.is_err() {
                self.note_fault();
            }
        }
        // Answering for a write is what lets its block leave memory, and nothing else about it changes: the
        // block has been in residency since it was closed. Completions may arrive in any order — a store
        // that models a device answers a write it refused ahead of ones the backing is still holding — and
        // that no longer matters here, because nothing is placed by it.
        self.trim_residency();
        any
    }

    /// Drops residency's oldest blocks back to its window, and stops at one whose write is outstanding.
    ///
    /// **That stop is the invariant, not a nicety.** A block evicted before the device has been given it
    /// would send the next read of it to a device that does not have it — and the read path goes there
    /// precisely *because* a block is not here. Residency may therefore sit above its window by as many
    /// blocks as the store will hold writes for, which is its queue depth and so a declared number.
    fn trim_residency(&mut self) {
        while self.resident.len() > self.resident_blocks {
            let oldest = self.oldest_resident;
            let held_by_write = |owed: &Owed| match owed {
                Owed::Block(unwritten) => unwritten.block == oldest,
                Owed::Free(_) => false,
            };
            if self
                .submitted_writes
                .iter()
                .any(|(_, owed)| held_by_write(owed))
                || self.pending_writes.iter().any(held_by_write)
            {
                break;
            }
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

/// What asking for one of a day's blocks did.
///
/// `Asked` is the ordinary answer once a day is older than residency, which it always is by the time it
/// expires: the read is on the queue and the records arrive with its completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walked {
    /// Visited now, from memory.
    Visited,
    /// Submitted. The completion carries the block, through `harvest`.
    Asked,
    /// The volume would not take another read, so nothing was asked and nothing moves this round.
    Busy,
    /// The day has no block there, which is how a walk knows it has reached the end.
    End,
}

/// What the log has done, for a report that has to say where the reads went.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogTraffic {
    pub appended: u64,
    pub died_in_buffer: u64,
    /// Survivors carried out of the writeback buffer into the block being packed. **Memory, not the
    /// device**: it was called `flushed`, and `flush` in this crate means reaching a device — which is
    /// what `flush_window_hours` means and what this never did. The load driver printed it as `carried
    /// on` to work around the name; the name says it now.
    pub carried_on: u64,
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
    use crate::testkit::HoldingStore;

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
        // Closing a block no longer writes it (§20): it has to be submitted and answered for before the
        // store can be asked to lie about it, which is what the worker's round does either side of a drain.
        log.submit_writes(0);
        log.collect_writes(0);
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
        assert!(modelled.submit_write(1, ObjectId::segment(0), 0, &block, true, 0));
        assert_eq!(modelled.poll_written(0), Some((1, Ok(()))));
        let mut into = Block::zeroed();

        assert!(modelled.submit(7, ObjectId::segment(0), 0, 0));
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
        assert!(stacked.submit_write(1, ObjectId::segment(0), 0, &block, true, 0));
        assert_eq!(stacked.poll_written(0), Some((1, Ok(()))));
        assert!(stacked.submit(9, ObjectId::segment(0), 0, 0));
        assert!(
            stacked.poll(0, &mut into).is_none(),
            "the outer model released a read the store below had not answered"
        );
        assert_eq!(stacked.poll(1_000, &mut into), Some(Ok(9)));
    }

    /// **A block that is not in the memory tier has already been written**, and a store answering out of
    /// order must not be able to change that.
    ///
    /// Residency is filled when a block is *closed*, so the queue is in block order by construction and a
    /// completion places nothing. It only opens eviction. Filling it on completion instead — which is what
    /// this once did — left a block closed, unwritten and unresident at the same time, and `LatencyStore`
    /// answering a write it refused ahead of ones the backing still held put residency out of order: a read
    /// of block 0 came back with block 1's first record.
    ///
    /// Every read here has to be answered and answered correctly, because none of these blocks may have left
    /// memory: the faults mean some of their writes never landed, and eviction is what waits on those.
    #[test]
    fn a_write_answered_out_of_order_does_not_shift_the_blocks_beside_it() {
        let model = StoreModel {
            // Every other call, so refusals land among writes the store below has taken.
            fault_every: 2,
            queue_depth: 8,
            ..StoreModel::default()
        };
        // Residency wide enough to hold every block, so what is asserted is where they sit in it.
        let mut log = RecordLog::new(model.build(OpenBacking::Memory, 5), 1, 64);
        let mut kept = Vec::new();
        for index in 0..RECORDS_PER_BLOCK * 6 {
            log.append(TxId(index as u128 + 1), &hold(10), ApplyIndex(1));
        }
        for index in 0..RECORDS_PER_BLOCK * 5 {
            let key = TxId(index as u128 + 1);
            kept.push((key, log.keep(key, &hold(10), ApplyIndex(1))));
        }
        log.submit_writes(0);
        log.collect_writes(0);

        // Residency here is wider than the blocks this makes, so nothing has been evicted and every one of
        // them must still answer from memory. That is a property of the sizing rather than a rule — the
        // rule is that a block *not* in memory has been written, and the next test is the one about that.
        for (key, addr) in &kept {
            let found = log.try_read(*addr);
            assert!(
                found.is_some(),
                "{addr:?} left memory although residency is wide enough to hold every block here"
            );
            assert_eq!(
                found.expect("just checked").0,
                *key,
                "{addr:?} came back with another block's record, so residency is not in block order"
            );
        }
    }

    /// **A block whose write is outstanding does not leave memory**, which is the one condition holding up
    /// the invariant that a block not in memory has been written.
    ///
    /// It needs a store that takes a write and does not answer for it, because `MemoryStore` answers as it
    /// takes and nothing is ever outstanding — so the gate is never reached and no other test here can meet
    /// it. Residency is one block wide against four, so without the gate three of them would be evicted
    /// into a device that has not been given them.
    #[test]
    fn a_block_whose_write_is_outstanding_does_not_leave_memory() {
        let store = HoldingStore::default();
        let mut log = RecordLog::new(Box::new(store.clone()), 1, 1);
        let mut kept = Vec::new();
        for index in 0..RECORDS_PER_BLOCK * 6 {
            log.append(TxId(index as u128 + 1), &hold(10), ApplyIndex(1));
        }
        // Five blocks' worth of survivors seals four: the fifth block's records are in the block being
        // packed, which is full and not yet closed.
        for index in 0..RECORDS_PER_BLOCK * 5 {
            let key = TxId(index as u128 + 1);
            kept.push((key, log.keep(key, &hold(10), ApplyIndex(1))));
        }
        log.submit_writes(0);
        log.collect_writes(0);
        assert_eq!(
            log.writes_outstanding(),
            4,
            "the store answered writes it was supposed to be holding"
        );

        // Residency is one block; four are outstanding, so all four have to still be here.
        for (key, addr) in &kept {
            let found = log.try_read(*addr);
            assert_eq!(
                found.map(|(found, _)| found),
                Some(*key),
                "{addr:?} left memory while its write was outstanding, so a read of it would go to a \
                 device that has not been given it"
            );
        }

        // Answered, and now the window applies: the oldest blocks go, and the store is what has them.
        store.release_all();
        log.collect_writes(0);
        assert_eq!(log.writes_outstanding(), 0);
        let (key, oldest) = kept[0];
        assert!(
            log.try_read(oldest).is_none(),
            "the window did not apply once the writes were answered"
        );
        assert_eq!(
            log.read(oldest).map(|(found, _)| found),
            Some(key),
            "a block the window dropped was not on the store, which is the whole of what the gate buys"
        );
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
