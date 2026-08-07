use std::collections::VecDeque;

use ledger_base::FxHashMap;

use crate::block::{Block, DurableStore, ObjectId, StoreFault, VolumeStats};

/// A read the caller is owed: its handle and the block it wants.
type Owed = (u64, ObjectId, u64);

/// A store that answers a read from the last blocks it read, and passes everything else down.
///
/// **A cold read cache, and residency is not one.** Residency keeps blocks by how recently they were
/// *written* — a day wide, oldest out first — so it holds what the ledger has just been doing. This holds
/// blocks by how recently they were *read*, which is a different set by construction: a block residency
/// still has never reaches a volume at all. They do not overlap and neither replaces the other.
///
/// **What it is for is a burst of reads into one block.** Expiry is that burst by construction: a block
/// holds fifty-one records, the sweep turns them into fifty-one voids in one slice, and every one of those
/// is judged with a lookup of a record on that same block. The day being emptied is older than residency by
/// definition, so all fifty-two of those reads reach the volume. Measured before this: 92,000 store reads
/// for 92,000 holds released.
///
/// **It has to sit where the read happens, not where it is asked for.** Keeping the last block read *at the
/// requester* was tried and bought nothing: the fifty-one lookups are all submitted before any completes, so
/// at submit time the block has not been read yet. Down here the reads are what they always were — one at a
/// time, in order — so the second one finds the first still in hand.
///
/// **Where it goes in the stack is the statement of what it caches.** Above `LatencyStore` a hit costs no
/// modelled device time, which is correct: the model prices a device, and a hit never reached one. Below it
/// the model would charge for reads that did not happen. So the composition is `Cached(LatencyStore(..))`
/// and not the other way round.
///
/// **No invalidation on a block, and that is a property rather than an omission.** A sealed block's bytes
/// never change — it is what the whole-block checksum rests on — and block numbers count on across days and
/// are never reused, so a number names one set of bytes for the life of the ledger. What does need clearing
/// is a whole *object*: a snapshot's partial is removed and written again under the same name, so anything
/// that writes, removes or renames drops that object's entries.
/// **Buffers are taken once and reused for ever.** A hash of a `u64` key is a multiply and a shift, and a
/// probe of a map this small stays in cache — neither is what rule 10 is about. What rule 10 is about is
/// allocation, and this crate has the scar: the idem stand-in's map rehashes as it crosses each power of
/// two, on the thread every request passes through, and it owns the worst tail of any long run. So the
/// buffers are a fixed array of slots, the map is sized once and keyed to slot numbers, and an eviction
/// hands a buffer back rather than dropping one.
pub struct Cached {
    inner: Box<dyn DurableStore>,
    /// The buffers, taken at construction. A slot is free exactly when no key maps to it.
    slots: Vec<Box<Block>>,
    /// Which slot holds which object's block. Sized for `blocks` and never grown.
    held: FxHashMap<(usize, u64), usize>,
    /// Slot numbers, least recently read first, so the ceiling evicts the oldest read rather than
    /// whatever the map happens to hand back.
    order: VecDeque<usize>,
    blocks: usize,
    /// Submitted reads this answered without going down, waiting for the `poll` that collects them. A
    /// submit cannot hand back bytes, so a hit becomes a completion that is already finished.
    ready: VecDeque<Owed>,
    /// Reads passed down, by the handle they were given, so a completion can be matched to its block.
    issued: FxHashMap<u64, (ObjectId, u64)>,
    /// Reads for a block that was already on its way down: the handle waiting, and what it wants. **This
    /// is the half a cache cannot do.** A cache answers "has this been read"; fifty-one lookups for one
    /// block are all submitted before any of them completes, so at submit time the answer is no for every
    /// one of them. This answers "is this being read", which is yes for fifty of the fifty-one.
    waiting: FxHashMap<(usize, u64), Vec<Owed>>,
    hits: u64,
    joined: u64,
}

impl Cached {
    /// Wraps a store with room for `blocks` of them. Zero is no cache at all, which is the baseline any
    /// number here is compared against.
    pub fn new(inner: Box<dyn DurableStore>, blocks: usize) -> Self {
        Self {
            inner,
            slots: (0..blocks).map(|_| Block::zeroed()).collect(),
            held: FxHashMap::with_capacity_and_hasher(blocks * 2, Default::default()),
            order: VecDeque::with_capacity(blocks),
            blocks,
            ready: VecDeque::with_capacity(blocks),
            issued: FxHashMap::default(),
            waiting: FxHashMap::default(),
            hits: 0,
            joined: 0,
        }
    }

    fn key(object: ObjectId, offset: u64) -> (usize, u64) {
        (object.index(), offset)
    }

    fn take_from_cache(&mut self, object: ObjectId, offset: u64, into: &mut Block) -> bool {
        let key = Self::key(object, offset);
        let Some(slot) = self.held.get(&key).copied() else {
            return false;
        };
        into.copy_from_slice(&self.slots[slot]);
        self.hits += 1;
        // Read again means kept longer, which is the whole of the policy.
        if let Some(at) = self.order.iter().position(|held| *held == slot) {
            self.order.remove(at);
            self.order.push_back(slot);
        }
        true
    }

    fn keep(&mut self, object: ObjectId, offset: u64, block: &Block) {
        if self.blocks == 0 {
            return;
        }
        let key = Self::key(object, offset);
        if self.held.contains_key(&key) {
            return;
        }
        let slot = if self.order.len() >= self.blocks {
            let oldest = self.order.pop_front().expect("a full cache has an oldest");
            self.held.retain(|_, at| *at != oldest);
            oldest
        } else {
            self.order.len()
        };
        self.slots[slot].copy_from_slice(block);
        self.held.insert(key, slot);
        self.order.push_back(slot);
    }

    /// Everything held for one object. A block's bytes never change, but an object's name can be given to
    /// different bytes — a snapshot's partial is removed and written again — so the unit of invalidation
    /// is the object and not the block. The buffers stay; only the claims on them go.
    fn forget(&mut self, object: ObjectId) {
        let mut dropped = Vec::new();
        self.held.retain(|(at, _), slot| {
            if *at == object.index() {
                dropped.push(*slot);
                return false;
            }
            true
        });
        self.order.retain(|slot| !dropped.contains(slot));
    }
}

impl DurableStore for Cached {
    fn submit_write(
        &mut self,
        handle: u64,
        object: ObjectId,
        offset: u64,
        block: &Block,
        creating: bool,
        now: u64,
    ) -> bool {
        self.forget(object);
        self.inner
            .submit_write(handle, object, offset, block, creating, now)
    }

    fn submit_barrier(&mut self, handle: u64, now: u64) -> bool {
        self.inner.submit_barrier(handle, now)
    }

    fn poll_written(&mut self, now: u64) -> Option<(u64, Result<(), StoreFault>)> {
        self.inner.poll_written(now)
    }

    fn writes_inflight(&self) -> usize {
        self.inner.writes_inflight()
    }

    fn writes_are_queued(&self) -> bool {
        self.inner.writes_are_queued()
    }

    fn read_at(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        if self.take_from_cache(object, offset, into) {
            return Ok(());
        }
        self.inner.read_at(object, offset, into)?;
        self.keep(object, offset, into);
        Ok(())
    }

    /// A hit is a completion that is already finished, kept until the caller comes for it — a submit has
    /// nowhere to put bytes.
    fn submit(&mut self, handle: u64, object: ObjectId, offset: u64, now: u64) -> bool {
        let key = Self::key(object, offset);
        if self.held.contains_key(&key) {
            self.ready.push_back((handle, object, offset));
            return true;
        }
        // Already on its way down. Waiting on it costs no read and no queue slot below, which is what the
        // fifty-one lookups of one expiry slice were spending.
        if let Some(waiters) = self.waiting.get_mut(&key) {
            waiters.push((handle, object, offset));
            self.joined += 1;
            return true;
        }
        if !self.inner.submit(handle, object, offset, now) {
            return false;
        }
        self.issued.insert(handle, (object, offset));
        self.waiting.insert(key, Vec::new());
        true
    }

    fn poll(&mut self, now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        if let Some((handle, object, offset)) = self.ready.pop_front() {
            if self.take_from_cache(object, offset, into) {
                return Some(Ok(handle));
            }
            // Evicted between the submit and the poll, so it becomes an ordinary read of the store — the
            // caller is owed an answer for that handle either way.
            return Some(
                self.inner
                    .read_at(object, offset, into)
                    .map(|()| handle)
                    .inspect(|_| self.keep(object, offset, into)),
            );
        }
        let answered = self.inner.poll(now, into)?;
        let Ok(handle) = answered else {
            // **A refused read names no handle**, so there is no way to tell which of the outstanding ones
            // came back — that was already true and is why a lookup that meets a fault simply stalls. What
            // coalescing adds is that a waiter must not be left joined to a read nobody will ever complete,
            // which would leave its block unaskable for the life of the node. So every claim goes. It costs
            // nothing that matters: a read fault seals the apply path (rule 19), and a node that will apply
            // nothing more has no lookups worth keeping.
            self.issued.clear();
            self.waiting.clear();
            return Some(answered);
        };
        let Some((object, offset)) = self.issued.remove(&handle) else {
            return Some(Ok(handle));
        };
        self.keep(object, offset, into);
        // Everyone who joined this read is now answerable from the block it brought back.
        if let Some(waiters) = self.waiting.remove(&Self::key(object, offset)) {
            self.ready.extend(waiters);
        }
        Some(Ok(handle))
    }

    fn inflight(&self) -> usize {
        self.ready.len() + self.inner.inflight()
    }

    fn take_charge(&mut self) -> u64 {
        self.inner.take_charge()
    }

    fn submit_remove(&mut self, handle: u64, object: ObjectId, now: u64) -> bool {
        self.forget(object);
        self.inner.submit_remove(handle, object, now)
    }

    fn submit_rename(&mut self, handle: u64, from: ObjectId, to: ObjectId, now: u64) -> bool {
        self.forget(from);
        self.forget(to);
        self.inner.submit_rename(handle, from, to, now)
    }

    fn blocks_in(&mut self, object: ObjectId) -> u64 {
        self.inner.blocks_in(object)
    }

    fn exists(&mut self, object: ObjectId) -> bool {
        self.inner.exists(object)
    }

    fn stats(&self) -> VolumeStats {
        let mut stats = self.inner.stats();
        stats.reads_cached = self.hits;
        stats.reads_joined = self.joined;
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemoryStore;

    fn store_with(blocks: u64) -> Box<dyn DurableStore> {
        let mut store = MemoryStore::default();
        let mut block = Block::zeroed();
        for at in 0..blocks {
            block[0] = at as u8;
            store.submit_write(
                at,
                ObjectId::segment(0),
                at * crate::block::BLOCK_BYTES as u64,
                &block,
                at == 0,
                0,
            );
        }
        while store.poll_written(0).is_some() {}
        Box::new(store)
    }

    /// The inline path: a second read of a block already in hand does not reach the store, and the block
    /// it answers with is the right one.
    #[test]
    fn a_block_read_twice_reaches_the_store_once() {
        let mut cached = Cached::new(store_with(2), 4);
        let mut into = Block::zeroed();
        for _ in 0..5 {
            cached
                .read_at(ObjectId::segment(0), 0, &mut into)
                .expect("the block is there");
            assert_eq!(into[0], 0, "the wrong block came back");
        }
        let stats = cached.stats();
        assert_eq!(stats.reads_inline, 1, "the store was asked more than once");
        assert_eq!(stats.reads_cached, 4);
    }

    /// **The queued path, which is the one a cache alone cannot serve.** Fifty-one lookups for one block
    /// are all submitted before any completes, so at submit time nothing has been read — the second and
    /// later ones join the first rather than asking again.
    #[test]
    fn reads_for_a_block_already_on_its_way_join_it() {
        let mut cached = Cached::new(store_with(2), 4);
        for handle in 0..8 {
            assert!(cached.submit(handle, ObjectId::segment(0), 0, 0));
        }
        let mut into = Block::zeroed();
        let mut answered = Vec::new();
        while let Some(done) = cached.poll(0, &mut into) {
            answered.push(done.expect("a block the store has"));
            assert_eq!(into[0], 0, "a waiter got the wrong block");
        }
        answered.sort_unstable();
        assert_eq!(answered, (0..8).collect::<Vec<_>>(), "a waiter was lost");
        let stats = cached.stats();
        assert_eq!(
            stats.reads_submitted, 1,
            "the store was asked for the same block more than once"
        );
        assert_eq!(stats.reads_joined, 7);
    }

    /// A different block is a different read: joining is keyed on the block, not on there being one in
    /// flight at all.
    #[test]
    fn a_read_of_another_block_is_its_own() {
        let mut cached = Cached::new(store_with(2), 4);
        assert!(cached.submit(1, ObjectId::segment(0), 0, 0));
        assert!(cached.submit(2, ObjectId::segment(0), crate::block::BLOCK_BYTES as u64, 0));
        let mut into = Block::zeroed();
        while cached.poll(0, &mut into).is_some() {}
        assert_eq!(cached.stats().reads_submitted, 2);
        assert_eq!(cached.stats().reads_joined, 0);
    }

    /// An object whose name is given to different bytes — a snapshot's partial, removed and written again
    /// — must not be answered from what the last one held. A block's bytes never change; an object's can.
    /// **A block's bytes never change; an object's can.** A snapshot's partial is removed and written
    /// again under the same name, so the unit of invalidation is the object — this is that life, and the
    /// second read must not be answered from the first one's bytes.
    #[test]
    fn an_object_written_again_drops_what_was_held_for_it() {
        let mut cached = Cached::new(Box::new(MemoryStore::default()), 4);
        let partial = ObjectId::SNAPSHOT_PARTIAL;
        let mut block = Block::zeroed();
        block[0] = 1;
        cached.submit_write(1, partial, 0, &block, true, 0);
        while cached.poll_written(0).is_some() {}

        let mut into = Block::zeroed();
        cached.read_at(partial, 0, &mut into).expect("it is there");
        assert_eq!(into[0], 1);
        assert_eq!(cached.stats().reads_inline, 1);

        cached.submit_remove(2, partial, 0);
        block[0] = 9;
        cached.submit_write(3, partial, 0, &block, true, 0);
        while cached.poll_written(0).is_some() {}

        cached.read_at(partial, 0, &mut into).expect("it is there");
        assert_eq!(
            into[0], 9,
            "the cache answered with the bytes the name held before"
        );
        assert_eq!(
            cached.stats().reads_inline,
            2,
            "the read did not reach the store"
        );
    }

    /// Zero blocks is the baseline: everything reaches the store, and nothing is remembered.
    #[test]
    fn a_cache_of_no_blocks_is_no_cache() {
        let mut cached = Cached::new(store_with(1), 0);
        let mut into = Block::zeroed();
        for _ in 0..3 {
            cached
                .read_at(ObjectId::segment(0), 0, &mut into)
                .expect("the block is there");
        }
        assert_eq!(cached.stats().reads_inline, 3);
        assert_eq!(cached.stats().reads_cached, 0);
    }
}
