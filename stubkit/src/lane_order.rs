use std::collections::VecDeque;
use std::time::Instant;

use ledger_base::{AccountId, FxHashMap};

struct Delayed<T, D> {
    due: D,
    value: T,
}

/// Contract 1 is enforced here, inside the external component: results leave in the lane's seq order
/// even when simulated latency would have finished them out of order. `violate_every` breaks it on
/// purpose so the sequencer's gap detection can be tested. The due time is a parameter so the same
/// ordering serves a stub running on real time and a simulation running on a virtual clock.
pub struct LaneOrderer<T, D = Instant> {
    lanes: FxHashMap<AccountId, VecDeque<Delayed<T, D>>>,
    rotation: VecDeque<AccountId>,
    /// Order-exempt results: they keep no place in any lane, so they leave as soon as their own
    /// simulated work is done, never behind anyone else's.
    exempt: VecDeque<Delayed<T, D>>,
    violate_every: u32,
    pushes: u32,
    items: usize,
}

impl<T, D: Ord + Copy> LaneOrderer<T, D> {
    pub fn new(violate_every: u32) -> Self {
        Self {
            lanes: FxHashMap::default(),
            rotation: VecDeque::new(),
            exempt: VecDeque::new(),
            violate_every,
            pushes: 0,
            items: 0,
        }
    }

    pub fn push(&mut self, lane: AccountId, due: D, value: T) {
        let queue = self.lanes.entry(lane).or_default();
        if queue.is_empty() {
            self.rotation.push_back(lane);
        }
        queue.push_back(Delayed { due, value });
        self.items += 1;

        self.pushes = self.pushes.wrapping_add(1);
        if self.violate_every > 0
            && self.pushes.is_multiple_of(self.violate_every)
            && queue.len() >= 2
        {
            let last = queue.len() - 1;
            queue.swap(last - 1, last);
        }
    }

    /// An order-exempt result: no lane, no place, so lane-ordering it would be order-wait bought for
    /// nothing. It still owes its own simulated latency, which is what the due time carries.
    pub fn push_unordered(&mut self, due: D, value: T) {
        self.exempt.push_back(Delayed { due, value });
        self.items += 1;
    }

    /// The earliest a lane head can leave, so a virtual clock can jump to it. Only lanes with
    /// something waiting are considered, which is what the rotation holds — the lane map keeps an
    /// entry per account it has ever seen.
    pub fn next_due(&self) -> Option<D> {
        self.rotation
            .iter()
            .filter_map(|lane| self.lanes.get(lane).and_then(|queue| queue.front()))
            .chain(self.exempt.front())
            .map(|head| head.due)
            .min()
    }

    /// Results held behind a lane head. This is what putting a lane back in order costs: with reads
    /// that complete out of order, a finished result waits for an earlier one on its lane, and the
    /// wait is the depth times a read's latency rather than one read's latency. Exempt results are
    /// never behind anyone, so only the one at the front of their queue counts as a head.
    pub fn behind_heads(&self) -> usize {
        let exempt_head = usize::from(!self.exempt.is_empty());
        self.items - self.rotation.len() - exempt_head
    }

    /// Head-of-lane only, which is what keeps a lane in order.
    pub fn pop_ready(&mut self, now: D) -> Option<T> {
        if let Some(head) = self.exempt.pop_front_if(|head| head.due <= now) {
            self.items -= 1;
            return Some(head.value);
        }
        for _ in 0..self.rotation.len() {
            let lane = self.rotation.pop_front()?;
            let queue = match self.lanes.get_mut(&lane) {
                Some(queue) => queue,
                None => continue,
            };
            let ready = queue.front().is_some_and(|head| head.due <= now);
            if !ready {
                self.rotation.push_back(lane);
                continue;
            }
            let item = queue.pop_front().map(|head| head.value);
            self.items -= 1;
            if !queue.is_empty() {
                self.rotation.push_back(lane);
            }
            return item;
        }
        None
    }
}
