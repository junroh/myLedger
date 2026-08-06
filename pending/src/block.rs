use std::collections::VecDeque;

use ledger_base::ports::{ApplyIndex, HoldData};
use ledger_base::{AccountId, Amount, BudgetGroup, FxHashMap, Prng, TxId};
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
pub struct BlockAddr(u64);

impl BlockAddr {
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

pub fn decode(bytes: &[u8], _from: BlockAddr) -> (TxId, HoldData) {
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

/// Whole blocks, written once. Below this is memory today; a file or a network volume goes here
/// without the engine above it changing, which is the point of the seam being bytes rather than
/// records — a store that had to understand a hold could not be either of those.
///
/// Two ways to read, because the engine has two callers with different constraints. A **lookup** submits
/// and harvests, so a store with a latency does not stop the loop. **Applying** a committed decision
/// cannot wait for anything — it is in order, and on a virtual clock a wait that only time can end never
/// ends — so it reads synchronously; a store that models a device charges its rate gate without holding
/// the caller, which prices the IO while leaving the latency of that path unmodelled. That is the gap a
/// read cache is meant to close, and it is why apply-path reads are counted separately.
pub trait BlockStore {
    fn write(&mut self, addr: BlockAddr, bytes: &[u8]);
    /// False when the block is not there, which is a bug rather than a miss: the index only points at
    /// blocks that were written.
    fn read(&self, addr: BlockAddr, into: &mut [u8]) -> bool;
    /// False when the store will not take another read yet, which is backpressure rather than failure.
    fn submit(&mut self, handle: u64, addr: BlockAddr, now: u64) -> bool;
    /// The next read finished by `now`, copied out. `None` while nothing is due.
    fn poll(&mut self, now: u64, into: &mut [u8]) -> Option<u64>;
    fn blocks(&self) -> usize;
    fn inflight(&self) -> usize;
    /// Drops every block of a segment and answers how many there were. The one way the store shrinks:
    /// blocks are written once and never rewritten, so space comes back a whole day at a time, and only
    /// once nothing in the index points into that day. The segment is in the address, so what belongs to a
    /// day is the store's own to find — no caller has to remember which blocks it handed over.
    fn free_segment(&mut self, segment: u8) -> usize;
}

/// The exact store: it keeps what it was given and adds no latency. Every other store is measured
/// against this one, and a simulation that wants a device's tail wraps it rather than replacing it.
#[derive(Default)]
pub struct MemBlockStore {
    blocks: FxHashMap<u64, Vec<u8>>,
    /// Submitted reads, answered in the order they were asked for and with no delay. A store that
    /// modelled a device would answer out of order; this one is the baseline that says what the
    /// structure does when the device is not the variable.
    submitted: VecDeque<(u64, BlockAddr)>,
}

impl MemBlockStore {
    fn key(addr: BlockAddr) -> u64 {
        BlockAddr::new(addr.segment(), addr.block(), 0).raw()
    }
}

impl BlockStore for MemBlockStore {
    fn write(&mut self, addr: BlockAddr, bytes: &[u8]) {
        self.blocks.insert(Self::key(addr), bytes.to_vec());
    }

    fn read(&self, addr: BlockAddr, into: &mut [u8]) -> bool {
        match self.blocks.get(&Self::key(addr)) {
            Some(block) => {
                into.copy_from_slice(block);
                true
            }
            None => false,
        }
    }

    fn submit(&mut self, handle: u64, addr: BlockAddr, _now: u64) -> bool {
        self.submitted.push_back((handle, addr));
        true
    }

    fn poll(&mut self, _now: u64, into: &mut [u8]) -> Option<u64> {
        let (handle, addr) = self.submitted.pop_front()?;
        self.read(addr, into).then_some(handle)
    }

    fn blocks(&self) -> usize {
        self.blocks.len()
    }

    fn inflight(&self) -> usize {
        self.submitted.len()
    }

    fn free_segment(&mut self, segment: u8) -> usize {
        let before = self.blocks.len();
        // The segment is in the key, so what belongs to a day is the store's own to find. A scan of the
        // map, which a day's worth of freeing can afford: it happens once per day and off any request's
        // path. A real device would have an extent per segment and free it in one call.
        self.blocks
            .retain(|key, _| BlockAddr::from_raw(*key).segment() != segment);
        before - self.blocks.len()
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

impl BlockAddr {
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
    bytes: Vec<u8>,
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
            bytes: vec![0; BLOCK_BYTES],
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

    fn get(&self, index: usize, addr: BlockAddr) -> Option<(TxId, HoldData)> {
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
pub struct LatencyBlockStore {
    inner: Box<dyn BlockStore>,
    device: Server,
    prng: Prng,
    /// Submitted reads and when the device says each is done, oldest first by admission. Completions are
    /// released by due time, which is not the order they were asked in.
    inflight: Vec<(u64, BlockAddr, u64)>,
    queue_depth: usize,
}

impl LatencyBlockStore {
    pub fn new(
        inner: Box<dyn BlockStore>,
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

impl BlockStore for LatencyBlockStore {
    fn write(&mut self, addr: BlockAddr, bytes: &[u8]) {
        self.inner.write(addr, bytes);
    }

    fn read(&self, addr: BlockAddr, into: &mut [u8]) -> bool {
        self.inner.read(addr, into)
    }

    fn submit(&mut self, handle: u64, addr: BlockAddr, now: u64) -> bool {
        if self.inflight.len() >= self.queue_depth {
            return false;
        }
        let due = self.device.serve(now, &mut self.prng);
        self.inflight.push((handle, addr, due));
        true
    }

    fn poll(&mut self, now: u64, into: &mut [u8]) -> Option<u64> {
        let at = self.inflight.iter().position(|(.., due)| *due <= now)?;
        let (handle, addr, _) = self.inflight.swap_remove(at);
        self.inner.read(addr, into).then_some(handle)
    }

    fn blocks(&self) -> usize {
        self.inner.blocks()
    }

    fn inflight(&self) -> usize {
        self.inflight.len()
    }

    /// Freeing costs the device nothing this model charges for: it is off any request's path, and a device
    /// that made it expensive would be one whose extents this store does not model.
    fn free_segment(&mut self, segment: u8) -> usize {
        self.inner.free_segment(segment)
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
    store: Box<dyn BlockStore>,
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
    scratch: Vec<u8>,
    /// Reads asked of the store and not yet answered, by handle, because a block carries several
    /// records and only the address says which one was wanted.
    fetching: FxHashMap<u64, BlockAddr>,
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
            Box::new(MemBlockStore::default()),
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
    pub fn new(store: Box<dyn BlockStore>, flush_blocks: usize, resident_blocks: usize) -> Self {
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
            scratch: vec![0; BLOCK_BYTES],
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
    pub fn append(&mut self, key: TxId, hold: &HoldData, at: ApplyIndex) -> BlockAddr {
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
        BlockAddr::buffered(ordinal, index as u8)
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
    /// Residency is not touched, and it does not have to be: it holds the most recently written blocks,
    /// and a day old enough to be freed left it long before. A configuration that kept records in memory
    /// longer than they are allowed to exist is refused at startup rather than handled here.
    pub fn free_segment(&mut self, segment: u8) -> usize {
        let freed = self.store.free_segment(segment);
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
        match self.buffer.front().filter(|block| block.filled > 0) {
            Some(oldest) => ApplyIndex(oldest.began_at.raw().saturating_sub(1)),
            None => applied_through,
        }
    }

    /// Whether this address names a record on a block the store has. False for a record in the writeback
    /// buffer, and false for one in the block still being filled: that block has handed out addresses and
    /// has not been written.
    ///
    /// A snapshot asks it about every slot it keeps. An index entry naming a block nobody has is worse than a
    /// hold the log can create again, so a slot pointing anywhere but a sealed block is written out empty.
    pub fn is_sealed(&self, addr: BlockAddr) -> bool {
        !addr.is_buffered() && addr.block() < self.next_block
    }

    /// Blocks this day wrote. Asked before freeing a day, because a store frees a segment by going looking
    /// for its blocks and that costs something even when there are none.
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
        visit: &mut dyn FnMut(TxId, HoldData, BlockAddr),
    ) -> bool {
        let Some(block) = self.days[segment as usize].block_at(at) else {
            return false;
        };
        if !self
            .store
            .read(BlockAddr::new(segment, block, 0), &mut self.scratch)
        {
            // The range says this block was written, so the store not having it is this node's own
            // bookkeeping disagreeing with itself. Nothing here can repair it; the walk goes on and the
            // day's count is what refuses to reach zero.
            return true;
        }
        self.store_reads += 1;
        for index in 0..RECORDS_PER_BLOCK {
            let addr = BlockAddr::new(segment, block, index as u8);
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
    pub fn oldest_block(&self) -> impl Iterator<Item = (TxId, HoldData, BlockAddr)> + '_ {
        let ordinal = self.oldest;
        let block = self.buffer.front();
        (0..RECORDS_PER_BLOCK).filter_map(move |index| {
            let addr = BlockAddr::buffered(ordinal, index as u8);
            block?.get(index, addr).map(|(key, hold)| (key, hold, addr))
        })
    }

    /// Writes a survivor on towards the store and answers its lasting address. The block it lands in
    /// is written out once full, so survivors of several buffered blocks share one — a block per flush
    /// would multiply the space a tenth of the records occupy.
    pub fn keep(&mut self, key: TxId, hold: &HoldData) -> BlockAddr {
        if self.store_open.full() {
            self.seal_store_block();
        }
        let index = self.store_open.put(key, hold);
        self.flushed += 1;
        BlockAddr::new(self.segment, self.next_block, index as u8)
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
    pub fn try_read(&mut self, addr: BlockAddr) -> Option<(TxId, HoldData)> {
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
    pub fn read(&mut self, addr: BlockAddr) -> Option<(TxId, HoldData)> {
        if let Some(found) = self.try_read(addr) {
            return Some(found);
        }
        self.apply_store_reads += 1;
        self.read_from_store(addr)
    }

    fn read_from_store(&mut self, addr: BlockAddr) -> Option<(TxId, HoldData)> {
        self.store_reads += 1;
        if !self.store.read(addr, &mut self.scratch) {
            return None;
        }
        let at = addr.index() as usize * RECORD_BYTES;
        Some(decode(&self.scratch[at..at + RECORD_BYTES], addr))
    }

    /// Asks the store for the block this address is on. False is backpressure: the store will not take
    /// another read yet.
    pub fn fetch(&mut self, handle: u64, addr: BlockAddr, now: u64) -> bool {
        if !self.store.submit(handle, addr, now) {
            return false;
        }
        self.fetching.insert(handle, addr);
        self.inflight_peak = self.inflight_peak.max(self.store.inflight());
        true
    }

    /// The next fetch finished by `now`. Completions may arrive in an order the reads were not asked
    /// in, which is what the orderer exists for.
    pub fn harvest(&mut self, now: u64) -> Option<(u64, BlockAddr, TxId, HoldData)> {
        let handle = self.store.poll(now, &mut self.scratch)?;
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
            self.store.blocks(),
        )
    }

    /// The block is durable now, and it stays readable anyway: the two windows are independent, and this
    /// is the one place that says so. Residency is trimmed here rather than on a schedule because this is
    /// the only event that adds to it.
    fn seal_store_block(&mut self) {
        let addr = BlockAddr::new(self.segment, self.next_block, 0);
        self.store.write(addr, &self.store_open.bytes);
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
        let (key, back) = decode(&bytes, BlockAddr::new(1, 2, 3));
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
        let addr = BlockAddr::new(63, (1 << BLOCK_BITS) - 1, 63);
        assert_eq!(
            (addr.segment(), addr.block(), addr.index()),
            (63, (1 << BLOCK_BITS) - 1, 63)
        );
        let modest = BlockAddr::new(2, 1_000_000, 7);
        assert_eq!(
            (modest.segment(), modest.block(), modest.index()),
            (2, 1_000_000, 7)
        );
        assert_eq!(BlockAddr::from_raw(modest.raw()), modest);
    }

    /// A record comes back from the block it landed in, whether that block is still being filled or
    /// has been sealed and handed to the store. Reading only one of the two would answer that a hold
    /// written moments ago does not exist.
    #[test]
    fn records_come_back_from_open_and_sealed_blocks_alike() {
        let mut log = RecordLog::default();
        let count = RECORDS_PER_BLOCK * 3 + 2;
        let addrs: Vec<BlockAddr> = (0..count)
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
