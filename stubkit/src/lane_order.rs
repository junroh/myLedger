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
    violate_every: u32,
    pushes: u32,
    items: usize,
}

impl<T, D: Ord + Copy> LaneOrderer<T, D> {
    pub fn new(violate_every: u32) -> Self {
        Self {
            lanes: FxHashMap::default(),
            rotation: VecDeque::new(),
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

    /// The earliest a lane head can leave, so a virtual clock can jump to it. Only lanes with
    /// something waiting are considered, which is what the rotation holds — the lane map keeps an
    /// entry per account it has ever seen.
    pub fn next_due(&self) -> Option<D> {
        self.rotation
            .iter()
            .filter_map(|lane| self.lanes.get(lane).and_then(|queue| queue.front()))
            .map(|head| head.due)
            .min()
    }

    /// Results held behind a lane head. This is what putting a lane back in order costs: with reads
    /// that complete out of order, a finished result waits for an earlier one on its lane, and the
    /// wait is the depth times a read's latency rather than one read's latency.
    pub fn behind_heads(&self) -> usize {
        self.items - self.rotation.len()
    }

    /// Head-of-lane only, which is what keeps a lane in order.
    pub fn pop_ready(&mut self, now: D) -> Option<T> {
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
