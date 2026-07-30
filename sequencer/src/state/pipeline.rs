//! Requests in flight: the queue they arrive on, the slots they live in, and what each one is
//! still waiting for.

use std::collections::VecDeque;
use std::mem::size_of;

use ledger_base::ports::{Correlation, IdemVerdict};
use ledger_base::{AccountId, AcctHandle, Consumer, Footprint, LinkedChainId, Peak, Request, Seq,
    Transfer, TransferKind};

/// Index into the work pool. Components see it only as an opaque correlation token.
pub type SlotId = u32;

/// External results still outstanding for a work item. It becomes judgeable at zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct DepFlags(u8);

impl DepFlags {
    pub const NONE: Self = Self(0);
    pub const IDEM: Self = Self(1 << 0);
    pub const PENDING: Self = Self(1 << 1);
    /// Waiting for a linked chain on the same lane.
    pub const LINKED_CHAIN: Self = Self(1 << 2);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

ledger_base::cache_aligned! {
/// A request in flight, in a slot reached by id. Padded to whole cache lines because it is read
/// four or five times per request at random.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkItem {
    pub tx: Transfer,
    /// Body fingerprint, so a retry can be told from a different request reusing the same id.
    pub digest: u64,
    /// Position in its lane, issued at intake.
    pub seq: Seq,
    /// The account whose order this seq belongs to: the debit side.
    pub lane: AccountId,
    pub kind: TransferKind,
    /// Resolved once at intake, so later stages never hash again.
    pub debit: AcctHandle,
    pub credit: AcctHandle,
    /// The linked chain this request belongs to, absent when it stands alone.
    pub chain: LinkedChainId,
    /// Outstanding external results; judgeable at zero.
    pub deps: DepFlags,
    /// Which have been sent, so a dispatch refused by a full queue resumes instead of resending.
    pub sent: DepFlags,
    /// What the idempotency engine said.
    pub verdict: Option<IdemVerdict>,
    /// The client's submit stamp, carried back untouched.
    pub submitted_at_nanos: u64,
}
}

ledger_base::layout_claim!(LAYOUT: WorkItem, size = 128, ledger_base::LineFit::WholeLines);

impl WorkItem {
    /// Whether this request holds a pin on its hold in the engine's overlay: only a kind that reads
    /// the hold takes one, and only once its pending step has actually gone through — a dispatch a
    /// full external queue refused took none and is retried. One predicate for both sides of the
    /// pair, because they were two: a request finished while its dispatch was still deferred was
    /// unpinned all the same, and that unpin comes off whatever pin the same hold has from another
    /// request in flight, letting eviction take an entry still to be read.
    pub const fn holds_pin(&self) -> bool {
        self.kind.needs_pending_lookup() && self.sent.contains(DepFlags::PENDING)
    }

    pub fn clear_dep(&mut self, dep: DepFlags) {
        self.deps = self.deps.without(dep);
    }

    pub const fn is_judgeable(&self) -> bool {
        self.deps.is_empty()
    }
}

/// Preallocated, so the pipeline never allocates per request.
pub struct SlotPool {
    items: Vec<WorkItem>,
    free: Vec<SlotId>,
    /// The most slots ever held at once. What sizes the pool, since the count at any moment is
    /// whatever the run happened to have in flight when it was asked.
    peak: Peak,
}

impl SlotPool {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: vec![WorkItem::default(); capacity],
            free: (0..capacity as SlotId).rev().collect(),
            peak: Peak::default(),
        }
    }

    pub fn alloc(&mut self) -> Option<SlotId> {
        let slot = self.free.pop();
        if slot.is_some() {
            self.peak.saw(self.in_flight());
        }
        slot
    }

    pub fn release(&mut self, slot: SlotId) {
        self.free.push(slot);
    }

    pub fn get(&self, slot: SlotId) -> &WorkItem {
        debug_assert!((slot as usize) < self.items.len());
        &self.items[slot as usize]
    }

    pub fn get_mut(&mut self, slot: SlotId) -> &mut WorkItem {
        debug_assert!((slot as usize) < self.items.len());
        &mut self.items[slot as usize]
    }

    pub fn in_flight(&self) -> usize {
        self.items.len() - self.free.len()
    }

    pub fn peak_in_flight(&self) -> usize {
        self.peak.entries()
    }

    /// The pool and its free list are one allocation as far as sizing goes: the list exists to hand
    /// out the slots, so a peak on it would just be the pool's peak upside down.
    fn footprint(&self, footprint: &mut Footprint) {
        let bytes = self.items.capacity() * size_of::<WorkItem>()
            + self.free.capacity() * size_of::<SlotId>();
        footprint.other(
            "work slots",
            self.in_flight(),
            self.peak.entries(),
            self.items.capacity(),
            bytes,
        );
    }

    pub const fn correlation(slot: SlotId) -> Correlation {
        Correlation(slot)
    }
}

/// The intake queue, the work slots, and the items a full external queue refused. A deferred item
/// keeps its lane seq and is retried; dropping it would leave a permanent gap in that lane.
pub struct Pipeline {
    requests: Consumer<Request>,
    slots: SlotPool,
    deferred: VecDeque<SlotId>,
    deferred_peak: Peak,
}

impl Pipeline {
    pub fn new(requests: Consumer<Request>, slots: usize, deferred: usize) -> Self {
        Self {
            requests,
            slots: SlotPool::with_capacity(slots),
            deferred: VecDeque::with_capacity(deferred),
            deferred_peak: Peak::default(),
        }
    }

    pub fn next_request(&self) -> Option<Request> {
        self.requests.pop()
    }

    pub fn alloc(&mut self) -> Option<SlotId> {
        self.slots.alloc()
    }

    pub fn release(&mut self, slot: SlotId) {
        self.slots.release(slot);
    }

    pub fn item(&self, slot: SlotId) -> &WorkItem {
        self.slots.get(slot)
    }

    pub fn item_mut(&mut self, slot: SlotId) -> &mut WorkItem {
        self.slots.get_mut(slot)
    }

    pub fn defer(&mut self, slot: SlotId) {
        self.deferred.push_back(slot);
        self.deferred_peak.saw(self.deferred.len());
    }

    /// The slots and the retry queue. Everything here is in flight, not working set: its size is what
    /// the run's component latencies made it, which is the part of a sizing answer no arithmetic gives.
    pub fn footprint(&self, footprint: &mut Footprint) {
        self.slots.footprint(footprint);
        footprint.buffer::<SlotId>(
            "deferred dispatches",
            self.deferred.len(),
            self.deferred.capacity(),
            self.deferred_peak.entries(),
        );
    }

    pub fn peak_in_flight(&self) -> usize {
        self.slots.peak_in_flight()
    }

    pub fn deferred_front(&self) -> Option<SlotId> {
        self.deferred.front().copied()
    }

    pub fn deferred_done(&mut self) {
        self.deferred.pop_front();
    }

    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    pub fn in_flight(&self) -> usize {
        self.slots.in_flight()
    }
}

#[cfg(test)]
mod tests {
    use super::{DepFlags, SlotPool, WorkItem};
    use ledger_base::TransferKind;

    /// A pin is taken when the pending step goes through, and released against the same fact. They
    /// used to be two conditions — the kind on one side, nothing on the other — so a request finished
    /// while a full external queue still had its dispatch deferred was unpinned without ever having
    /// been pinned.
    #[test]
    fn a_request_whose_dispatch_never_went_through_holds_no_pin() {
        let mut item = WorkItem { kind: TransferKind::Settle, ..WorkItem::default() };
        assert!(!item.holds_pin(), "nothing has been sent yet");
        item.sent = item.sent.with(DepFlags::PENDING);
        assert!(item.holds_pin(), "the pending step went through, so a pin was taken");

        let inline = WorkItem {
            kind: TransferKind::SinglePhase,
            sent: DepFlags::NONE.with(DepFlags::PENDING),
            ..WorkItem::default()
        };
        assert!(!inline.holds_pin(), "a kind that reads no hold never pins one");
    }

    /// Why a watermark exists at all: a sizing answer taken from the current count reports whatever
    /// the run happened to be holding when it was asked, which for a drained pool is nothing.
    #[test]
    fn the_slot_peak_is_the_most_ever_held_not_the_most_held_now() {
        let mut pool = SlotPool::with_capacity(4);
        let first = pool.alloc().expect("a free slot");
        let second = pool.alloc().expect("a free slot");
        pool.release(first);
        pool.release(second);
        assert_eq!(pool.in_flight(), 0, "the pool drained");
        assert_eq!(pool.peak_in_flight(), 2, "releasing a slot must not lower the peak");
    }

    /// A pool cannot report having held more than it has, which is what makes the peak comparable to
    /// the size the run was given.
    #[test]
    fn the_slot_peak_never_exceeds_the_pool() {
        let mut pool = SlotPool::with_capacity(2);
        for _ in 0..8 {
            let _ = pool.alloc();
        }
        assert_eq!(pool.peak_in_flight(), 2);
    }
}
