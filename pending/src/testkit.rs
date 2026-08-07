//! Stand-ins two test modules both need, and nothing a build ships.
//!
//! One store double rather than one per module (rule 1): "a device that answers when it is told to" is the
//! same thing whether what is being tested is eviction or a snapshot's rename, and two copies of it would
//! drift apart in exactly the property both tests turn on.

use std::collections::VecDeque;

use crate::block::{Block, DurableStore, MemoryStore, ObjectId, StoreFault};

/// A store that takes writes and answers for them only when asked.
///
/// `MemoryStore` answers as it takes, so nothing is ever outstanding under it and the eviction gate is
/// unreachable. Shared through an `Rc` for the same reason the snapshot tests' store is: the log owns
/// the box, and the test has to reach past it to say when the device replies.
#[derive(Clone, Default)]
pub struct HoldingStore(std::rc::Rc<std::cell::RefCell<Holding>>);

/// What the store was asked to do, in the order it was asked. **Order is the queue's promise**, so a test
/// about it has to be able to see the sequence rather than only the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Write,
    Barrier,
    Remove,
    Rename,
}

#[derive(Default)]
struct Holding {
    inner: MemoryStore,
    held: VecDeque<(u64, Result<(), StoreFault>)>,
    release: usize,
    ops: Vec<StoreOp>,
}

impl HoldingStore {
    pub fn release_all(&self) {
        let mut held = self.0.borrow_mut();
        held.release = held.held.len();
    }
}

impl DurableStore for HoldingStore {
    fn submit_write(
        &mut self,
        handle: u64,
        object: ObjectId,
        offset: u64,
        block: &Block,
        creating: bool,
        now: u64,
    ) -> bool {
        let mut held = self.0.borrow_mut();
        if !held
            .inner
            .submit_write(handle, object, offset, block, creating, now)
        {
            return false;
        }
        held.ops.push(StoreOp::Write);
        while let Some(done) = held.inner.poll_written(now) {
            held.held.push_back(done);
        }
        true
    }

    fn submit_barrier(&mut self, handle: u64, now: u64) -> bool {
        let mut held = self.0.borrow_mut();
        if !held.inner.submit_barrier(handle, now) {
            return false;
        }
        held.ops.push(StoreOp::Barrier);
        while let Some(done) = held.inner.poll_written(now) {
            held.held.push_back(done);
        }
        true
    }

    fn poll_written(&mut self, _now: u64) -> Option<(u64, Result<(), StoreFault>)> {
        let mut held = self.0.borrow_mut();
        if held.release == 0 {
            return None;
        }
        let done = held.held.pop_front()?;
        held.release -= 1;
        Some(done)
    }

    fn writes_inflight(&self) -> usize {
        self.0.borrow().held.len()
    }

    fn read_at(
        &mut self,
        object: ObjectId,
        offset: u64,
        into: &mut Block,
    ) -> Result<(), StoreFault> {
        self.0.borrow_mut().inner.read_at(object, offset, into)
    }

    fn submit(&mut self, handle: u64, object: ObjectId, offset: u64, now: u64) -> bool {
        self.0
            .borrow_mut()
            .inner
            .submit(handle, object, offset, now)
    }

    fn poll(&mut self, now: u64, into: &mut Block) -> Option<Result<u64, StoreFault>> {
        self.0.borrow_mut().inner.poll(now, into)
    }

    fn inflight(&self) -> usize {
        self.0.borrow().inner.inflight()
    }

    /// Held like every other answer: a namespace change is on the same queue as the writes, so a double
    /// that answered it at once would be describing a different store.
    fn submit_remove(&mut self, handle: u64, object: ObjectId, now: u64) -> bool {
        let mut held = self.0.borrow_mut();
        if !held.inner.submit_remove(handle, object, now) {
            return false;
        }
        held.ops.push(StoreOp::Remove);
        while let Some(done) = held.inner.poll_written(now) {
            held.held.push_back(done);
        }
        true
    }

    fn submit_rename(&mut self, handle: u64, from: ObjectId, to: ObjectId, now: u64) -> bool {
        let mut held = self.0.borrow_mut();
        if !held.inner.submit_rename(handle, from, to, now) {
            return false;
        }
        held.ops.push(StoreOp::Rename);
        while let Some(done) = held.inner.poll_written(now) {
            held.held.push_back(done);
        }
        true
    }

    fn blocks_in(&mut self, object: ObjectId) -> u64 {
        self.0.borrow_mut().inner.blocks_in(object)
    }

    fn exists(&mut self, object: ObjectId) -> bool {
        self.0.borrow_mut().inner.exists(object)
    }

    fn stats(&self) -> crate::block::VolumeStats {
        self.0.borrow().inner.stats()
    }
}

impl HoldingStore {
    /// Answers everything held and everything submitted after it. What `release_all` is not: a test that
    /// has to see a dump *finish* cannot release a fixed number, because the barrier it is waiting for has
    /// not been submitted yet.
    pub fn stop_holding(&self) {
        self.0.borrow_mut().release = usize::MAX;
    }

    /// Whether the store has this object, asked past the box the caller owns.
    pub fn holds(&self, object: ObjectId) -> bool {
        self.0.borrow_mut().inner.exists(object)
    }

    /// Everything asked of the store, in order.
    pub fn ops(&self) -> Vec<StoreOp> {
        self.0.borrow().ops.clone()
    }
}
