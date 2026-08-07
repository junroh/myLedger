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

#[derive(Default)]
struct Holding {
    inner: MemoryStore,
    held: VecDeque<(u64, Result<(), StoreFault>)>,
    release: usize,
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

    fn remove(&mut self, object: ObjectId) -> Result<(), StoreFault> {
        self.0.borrow_mut().inner.remove(object)
    }

    fn rename(&mut self, from: ObjectId, to: ObjectId) -> Result<(), StoreFault> {
        self.0.borrow_mut().inner.rename(from, to)
    }

    fn exists(&mut self, object: ObjectId) -> bool {
        self.0.borrow_mut().inner.exists(object)
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
}
