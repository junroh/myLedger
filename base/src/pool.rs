//! Recycled `Vec` buffers, so a pipeline that hands buffers to another thread and gets them back
//! never allocates in its steady state.

use std::mem::size_of;

/// A small set of spare buffers, handed out and returned.
pub struct BufferPool<T> {
    spare: Vec<Vec<T>>,
    capacity: usize,
    limit: usize,
}

impl<T> BufferPool<T> {
    pub fn new(buffers: usize, capacity: usize) -> Self {
        Self {
            spare: (0..buffers).map(|_| Vec::with_capacity(capacity)).collect(),
            capacity,
            limit: buffers,
        }
    }

    pub fn take(&mut self) -> Vec<T> {
        self.spare
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(self.capacity))
    }

    /// Buffers held spare, and the bytes behind them. A pool of `in_flight + 1` batch buffers is real
    /// memory the owner has to report: it is preallocated exactly so the steady state never allocates,
    /// which means it is always there.
    pub fn held(&self) -> usize {
        self.spare.len()
    }

    pub fn bytes(&self) -> usize {
        self.spare
            .iter()
            .map(|buffer| buffer.capacity())
            .sum::<usize>()
            * size_of::<T>()
    }

    /// Returned empty, ready to be handed out again. A pool that kept everything handed back would
    /// be an unbounded queue, so anything past the limit is dropped instead.
    pub fn give(&mut self, mut buffer: Vec<T>) {
        if self.spare.len() >= self.limit {
            return;
        }
        buffer.clear();
        self.spare.push(buffer);
    }
}
