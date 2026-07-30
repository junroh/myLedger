use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::layout::CachePadded;

/// Bounded single-producer single-consumer queue. Every external path owns its own
/// queue so a slow path cannot delay a fast one, and each queue keeps exactly one
/// writer and one reader.
struct Ring<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
}

// Safety: `Ring` hands out one `Producer` and one `Consumer`; the producer only writes
// slots in [tail, head + capacity) and the consumer only reads slots in [head, tail).
unsafe impl<T: Send> Send for Ring<T> {}
unsafe impl<T: Send> Sync for Ring<T> {}

impl<T> Ring<T> {
    fn capacity(&self) -> usize {
        self.mask + 1
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        let mut index = head;
        while index != tail {
            // Safety: slots in [head, tail) are initialised and no longer observed.
            unsafe { (*self.slots[index & self.mask].get()).assume_init_drop() };
            index = index.wrapping_add(1);
        }
    }
}

pub struct Producer<T> {
    ring: Arc<Ring<T>>,
    tail: Cell<usize>,
    cached_head: Cell<usize>,
}

pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
    head: Cell<usize>,
    cached_tail: Cell<usize>,
}

pub fn channel<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let capacity = capacity.next_power_of_two();
    let mut slots = Vec::with_capacity(capacity);
    slots.resize_with(capacity, || UnsafeCell::new(MaybeUninit::uninit()));
    let ring = Arc::new(Ring {
        slots: slots.into_boxed_slice(),
        mask: capacity - 1,
        head: CachePadded::new(AtomicUsize::new(0)),
        tail: CachePadded::new(AtomicUsize::new(0)),
    });
    (
        Producer {
            ring: Arc::clone(&ring),
            tail: Cell::new(0),
            cached_head: Cell::new(0),
        },
        Consumer {
            ring,
            head: Cell::new(0),
            cached_tail: Cell::new(0),
        },
    )
}

impl<T> Producer<T> {
    pub fn push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.get();
        if tail.wrapping_sub(self.cached_head.get()) == self.ring.capacity() {
            self.cached_head.set(self.ring.head.load(Ordering::Acquire));
            if tail.wrapping_sub(self.cached_head.get()) == self.ring.capacity() {
                return Err(value);
            }
        }
        // Safety: the slot is free (tail - head < capacity) and only this producer writes it.
        unsafe { (*self.ring.slots[tail & self.ring.mask].get()).write(value) };
        self.ring
            .tail
            .store(tail.wrapping_add(1), Ordering::Release);
        self.tail.set(tail.wrapping_add(1));
        Ok(())
    }

    /// Publishes a whole batch with a single release store. `fill` writes straight into the
    /// ring, so a batch costs one copy rather than a staging buffer plus a copy. Returns how
    /// many were taken, so the caller can retry the remainder.
    pub fn push_from<F>(&self, count: usize, mut fill: F) -> usize
    where
        F: FnMut(usize) -> T,
    {
        let tail = self.tail.get();
        let mut free = self.ring.capacity() - tail.wrapping_sub(self.cached_head.get());
        if free < count {
            self.cached_head.set(self.ring.head.load(Ordering::Acquire));
            free = self.ring.capacity() - tail.wrapping_sub(self.cached_head.get());
        }
        let taken = free.min(count);
        for offset in 0..taken {
            let index = tail.wrapping_add(offset) & self.ring.mask;
            // Safety: the slot is free and only this producer writes it.
            unsafe { (*self.ring.slots[index].get()).write(fill(offset)) };
        }
        self.ring
            .tail
            .store(tail.wrapping_add(taken), Ordering::Release);
        self.tail.set(tail.wrapping_add(taken));
        taken
    }

    pub fn len(&self) -> usize {
        self.ring
            .tail
            .load(Ordering::Relaxed)
            .wrapping_sub(self.ring.head.load(Ordering::Relaxed))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
pub struct StagedProducer<T> {
    producer: Producer<T>,
    staged: Option<T>,
}

impl<T> StagedProducer<T> {
    pub const fn new(producer: Producer<T>) -> Self {
        Self {
            producer,
            staged: None,
        }
    }

    pub fn is_stuck(&self) -> bool {
        self.staged.is_some()
    }

    pub fn send(&mut self, value: T) {
        debug_assert!(!self.is_stuck(), "send while stuck loses ordering");
        if let Err(rejected) = self.producer.push(value) {
            self.staged = Some(rejected);
        }
    }

    /// Returns true when the stage is clear and sending may resume.
    pub fn flush(&mut self) -> bool {
        match self.staged.take() {
            None => true,
            Some(value) => {
                if let Err(rejected) = self.producer.push(value) {
                    self.staged = Some(rejected);
                    return false;
                }
                true
            }
        }
    }
}

impl<T> Consumer<T> {
    pub fn pop(&self) -> Option<T> {
        let head = self.head.get();
        if head == self.cached_tail.get() {
            self.cached_tail.set(self.ring.tail.load(Ordering::Acquire));
            if head == self.cached_tail.get() {
                return None;
            }
        }
        // Safety: the slot is initialised (head < tail) and only this consumer reads it.
        let value = unsafe { (*self.ring.slots[head & self.ring.mask].get()).assume_init_read() };
        self.ring
            .head
            .store(head.wrapping_add(1), Ordering::Release);
        self.head.set(head.wrapping_add(1));
        Some(value)
    }

    pub fn len(&self) -> usize {
        self.ring
            .tail
            .load(Ordering::Relaxed)
            .wrapping_sub(self.ring.head.load(Ordering::Relaxed))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring holds exactly its capacity, hands values back in order across the wrap point, and
    /// refuses by giving the value back rather than dropping it.
    #[test]
    fn a_full_ring_refuses_and_wraps_in_order() {
        let (tx, rx) = channel::<u32>(4);
        for value in 0..4 {
            assert_eq!(tx.push(value), Ok(()));
        }
        assert_eq!(tx.push(99), Err(99), "a full ring must give the value back");
        assert_eq!(rx.pop(), Some(0));
        assert_eq!(tx.push(4), Ok(()), "one slot came free");

        let drained: Vec<u32> = std::iter::from_fn(|| rx.pop()).collect();
        assert_eq!(drained, vec![1, 2, 3, 4], "order survives the wrap");
        assert_eq!(rx.pop(), None);
    }

    /// A batch takes only what fits and says how much it took, so the caller can retry the rest.
    /// `fill` is called with the offset inside the batch, not the slot.
    #[test]
    fn a_batch_takes_what_fits_and_reports_it() {
        let (tx, rx) = channel::<usize>(4);
        assert_eq!(tx.push(0), Ok(()));

        let taken = tx.push_from(8, |offset| offset + 1);
        assert_eq!(taken, 3, "one slot was already used");

        let drained: Vec<usize> = std::iter::from_fn(|| rx.pop()).collect();
        assert_eq!(drained, vec![0, 1, 2, 3]);
    }

    /// The point of the whole structure: one thread writes, another reads, and every value arrives
    /// once and in order. This is what the release and acquire pairing is for.
    #[test]
    fn a_producer_thread_and_a_consumer_thread_agree_on_every_value() {
        const COUNT: u64 = 200_000;
        let (tx, rx) = channel::<u64>(1024);
        let writer = std::thread::spawn(move || {
            let mut value = 0;
            while value < COUNT {
                if tx.push(value).is_ok() {
                    value += 1;
                }
            }
        });

        let mut expected = 0;
        while expected < COUNT {
            if let Some(value) = rx.pop() {
                assert_eq!(value, expected, "out of order or duplicated");
                expected += 1;
            }
        }
        writer.join().expect("writer");
        assert_eq!(rx.pop(), None);
    }

    /// Values left in the ring are owned by it: dropping the ring drops them, exactly once.
    #[test]
    fn dropping_the_ring_drops_what_is_still_in_it() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = channel::<Counted>(4);
        for _ in 0..3 {
            tx.push(Counted(Arc::clone(&drops))).ok().expect("room");
        }
        drop(rx.pop());
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        drop((tx, rx));
        assert_eq!(drops.load(Ordering::Relaxed), 3, "the ring dropped what was left");
    }
}
