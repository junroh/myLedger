use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};

use ledger_base::ports::{ApplyIndex, HoldData};
use ledger_base::{AccountId, Amount, BudgetGroup, FxHashMap, LineFit, Prng, TxId};
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

impl Block {
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

/// What a store can fail at.
///
/// One variant, because without a device only one thing can go wrong: the store is asked for a block it
/// does not have. That is not a miss — the index only ever names blocks that were written — so it is this
/// node's own record of where blocks are having stopped agreeing with the store. A device that refuses a
/// read for reasons of its own is a second variant, and it arrives with the device that can produce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFault {
    Missing,
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
/// and harvests, so a store with a latency does not stop the loop. **Applying** a committed decision
/// cannot wait for anything — it is in order, and on a virtual clock a wait that only time can end never
/// ends — so it reads synchronously; a store that models a device charges its rate gate without holding
/// the caller, which prices the IO while leaving the latency of that path unmodelled. That is the gap a
/// read cache is meant to close, and it is why apply-path reads are counted separately.
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
    blocks: Vec<Box<Block>>,
}

impl SegmentFile {
    fn at(&self, offset: u64) -> Option<&Block> {
        let within = offset.checked_sub(self.base)?;
        self.blocks
            .get((within / BLOCK_BYTES as u64) as usize)
            .map(|block| &**block)
    }

    fn end(&self) -> u64 {
        self.base + self.blocks.len() as u64 * BLOCK_BYTES as u64
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
            blocks: vec![Block::copy_of(block)],
        });
        Ok(())
    }

    fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        let file = self.segments[segment as usize]
            .as_mut()
            .ok_or(StoreFault::Missing)?;
        // Blocks are written once and a segment's own are consecutive, so its end is the only offset a
        // write can have. A self-invariant, not a fault: the caller's block numbers and this sequence are
        // both ours, and disagreeing means one of them is wrong rather than the store being broken.
        debug_assert_eq!(
            offset,
            file.end(),
            "a block goes at the end of its segment or nowhere"
        );
        file.blocks.push(Block::copy_of(block));
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

/// A store with a device's timing in front of another store. What makes it a stand-in is
/// `stubkit::Server` — a rate gate with an exponential tail, so reads are admitted no faster than the
/// ceiling and each draws its own latency, which is what makes them complete **out of the order they
/// were asked for**. It lives here rather than in `stubkit` only because the trait does and the
/// dependency runs this way.
///
/// The synchronous read charges the device without waiting: the path that applies committed decisions is
/// in order, and on a virtual clock a wait only time can end never ends. So this prices that path's IO
/// and leaves its latency unmodelled — stated, because it is the one number this cannot answer.
///
/// **It wraps any store, including one with a real device under it, and the composition is a floor rather
/// than a sum.** A completion is an absolute time taken from when the read was admitted, so when the inner
/// store is memory the drawn time is the whole of it, and when the inner store is real its own time has
/// already passed by the time the deadline is compared — whichever is slower wins, with nothing measuring
/// the difference. That is what makes "model a device slower than the one I have" mean something, and
/// modelling a faster one impossible.
pub struct LatencyStore {
    inner: Box<dyn DurableStore>,
    device: Server,
    prng: Prng,
    /// Submitted reads and when the device says each is done, oldest first by admission. Completions are
    /// released by due time, which is not the order they were asked in.
    inflight: Vec<(u64, u8, u64, u64)>,
    queue_depth: usize,
}

impl LatencyStore {
    pub fn new(
        inner: Box<dyn DurableStore>,
        base_nanos: u64,
        tail_nanos: u64,
        per_second: u64,
        queue_depth: usize,
        seed: u64,
    ) -> Self {
        Self {
            inner,
            device: Server::new(base_nanos, tail_nanos, per_second),
            prng: Prng::new(seed),
            inflight: Vec::with_capacity(queue_depth),
            queue_depth: queue_depth.max(1),
        }
    }
}

impl DurableStore for LatencyStore {
    fn open_with(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        self.inner.open_with(segment, offset, block)
    }

    fn append(&mut self, segment: u8, offset: u64, block: &Block) -> Result<(), StoreFault> {
        self.inner.append(segment, offset, block)
    }

    fn read_at(&mut self, segment: u8, offset: u64, into: &mut Block) -> Result<(), StoreFault> {
        self.inner.read_at(segment, offset, into)
    }

    fn submit(&mut self, handle: u64, segment: u8, offset: u64, now: u64) -> bool {
        if self.inflight.len() >= self.queue_depth {
            return false;
        }
        let due = self.device.serve(now, &mut self.prng);
        self.inflight.push((handle, segment, offset, due));
        true
    }

    fn poll(&mut self, now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        let at = self.inflight.iter().position(|(.., due)| *due <= now)?;
        let (handle, segment, offset, _) = self.inflight.swap_remove(at);
        Some(self.inner.read_at(segment, offset, into).map(|()| handle))
    }

    fn inflight(&self) -> usize {
        self.inflight.len()
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
            scratch: Block::zeroed(),
            fetching: FxHashMap::default(),
            appended: 0,
            died_in_buffer: 0,
            flushed: 0,
            freed: 0,
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

    /// The last log position everything up to which has reached a block, given the position of the batch
    /// being applied now.
    ///
    /// It is the oldest buffered block's own position minus one, because that block holds the first record
    /// that has *not* been sealed — so a snapshot claiming to cover its batch would be claiming a record it
    /// does not carry. With nothing buffered, everything applied has been sealed and the answer is the
    /// caller's own position.
    ///
    /// Position zero means it covers nothing, which is a legitimate answer rather than a missing one: a
    /// snapshot of an engine that has applied nothing is what a follower starting from empty receives.
    pub fn sealed_through(&self, applied_through: ApplyIndex) -> ApplyIndex {
        // The block being filled comes first when it has anything in it, because its records are *out of the
        // buffer and not on the store*. Reading only the buffer was a real defect: coverage claimed a
        // hundred and fifty-three while the records of position a hundred and three sat in this block, so the
        // snapshot left their slots out and replay started after them. The holds were simply gone.
        //
        // Its stamp comes from the buffered block compaction drained into it, which is a lower bound on its
        // survivors' own positions — conservative in the safe direction.
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

    /// Whether this address names a record on a block the store has. False for a record in the writeback
    /// buffer, and false for one in the block still being filled: that block has handed out addresses and
    /// has not been written.
    ///
    /// A snapshot asks it about every slot it keeps. An index entry naming a block nobody has is worse than a
    /// hold the log can create again, so a slot pointing anywhere but a sealed block is written out empty.
    pub fn is_sealed(&self, addr: RecordAddr) -> bool {
        !addr.is_buffered() && addr.block() < self.next_block
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
        let handle = self.store.poll(now, &mut self.scratch)?.ok()?;
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

    /// The block is durable now, and it stays readable anyway: the two windows are independent, and this
    /// is the one place that says so. Residency is trimmed here rather than on a schedule because this is
    /// the only event that adds to it.
    fn seal_store_block(&mut self) {
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
        let _ = written;
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
