use std::collections::VecDeque;

use ledger_base::{AccountId, FxHashMap, Seq, UNORDERED};

/// Contract 1 is the engine's to keep: a lane's replies leave in the seq order the sequencer issued,
/// however they completed. The sequencer only checks, so anything that leaves here out of order becomes
/// a gap and quarantines the lane.
///
/// A place is reserved when the command is taken off the queue and filled when its work finishes, which
/// is what a read completing out of order requires: releasing in the order things *finished* would be
/// the device's order, not the lane's.
///
/// The engine cannot guess a lane's numbering. It sees only a subsequence of a lane's seqs — a hold or a
/// single-phase transfer never travels this path at all — so a run of commands starts wherever the
/// sequencer's fence rule makes it start, and the first place reserved after a lane falls quiet is what
/// defines it.
pub struct Orderer<T> {
    lanes: FxHashMap<AccountId, Lane<T>>,
    /// Lanes with something waiting, so releasing does not walk every lane the engine has ever seen.
    rotation: VecDeque<AccountId>,
    held: usize,
    /// The deepest a *single lane* got. The total held says nothing about ordering — most of it is
    /// replies waiting for their own work — and the product that matters is lane depth times read
    /// latency.
    deepest_lane: usize,
    /// Break the contract on purpose, so the sequencer's gap detection can be tested. A fault, and the
    /// only reason this knob exists.
    violate_every: u32,
    pushes: u32,
    released: u64,
    /// Split at fill, because the two have different cures. A reply filled behind a place that is still
    /// empty is waiting for the *lane*: that is order-wait, and exemption is what removes it. A reply
    /// filled as its lane's head is already free to leave, so whatever it then waits is this loop's own
    /// delay and the reactor's backpressure — a faster engine or a faster drain, not ordering.
    order_nanos: u64,
    order_worst_nanos: u64,
    delivery_nanos: u64,
    order_held: u64,
}

struct Lane<T> {
    /// The seq this lane may release next. Set from the first place reserved after the lane falls
    /// quiet, because that is the only moment the engine can know where a run begins.
    next: Seq,
    /// Reserved in command order; `None` until the work behind it finishes.
    places: FxHashMap<Seq, Option<Held<T>>>,
    /// Replies that keep no place in the lane. A map keyed by seq could not hold them: they all carry
    /// the same absent seq, so each would overwrite the last.
    exempt: VecDeque<Held<T>>,
    /// Where the fault has sent a reply that has not arrived yet. Two places trade contents, so both
    /// are still filled and still released — only in the wrong order, which is the point.
    swapped: FxHashMap<Seq, Seq>,
    in_rotation: bool,
}

struct Held<T> {
    /// When the work behind this reply finished. What it waits for after that is the lane's order, and
    /// keeping the two apart is the only way to see what ordering costs.
    ready_at: u64,
    /// Whether an earlier seq on its lane had still to go when this arrived. Decided here, once, because
    /// afterwards there is no way to tell a reply that waited for its lane from one that waited for the
    /// loop to come round to it.
    blocked: bool,
    value: T,
}

impl<T> Orderer<T> {
    pub fn new(violate_every: u32) -> Self {
        Self {
            lanes: FxHashMap::default(),
            rotation: VecDeque::new(),
            held: 0,
            deepest_lane: 0,
            violate_every,
            pushes: 0,
            released: 0,
            order_nanos: 0,
            order_worst_nanos: 0,
            delivery_nanos: 0,
            order_held: 0,
        }
    }

    /// Reserves this command's place, in the order the commands were taken off the queue. Ordered
    /// commands only: an exempt one keeps no place, so there is nothing to reserve.
    pub fn expect(&mut self, lane: AccountId, seq: Seq) {
        if seq == UNORDERED {
            return;
        }
        let entry = self.lane_mut(lane);
        if entry.places.is_empty() {
            entry.next = seq;
        }
        entry.places.insert(seq, None);
        self.enrol(lane);
    }

    /// Hands a finished reply over. It leaves when the lane's earlier places have left and its own work
    /// is done, not before.
    pub fn fill(&mut self, lane: AccountId, seq: Seq, ready_at: u64, value: T) {
        self.pushes = self.pushes.wrapping_add(1);
        // The fault: claim the place the lane is waiting for, so this reply overtakes whatever holds it.
        let violate = self.violate_every > 0
            && self.pushes.is_multiple_of(self.violate_every)
            && seq != UNORDERED;
        let entry = self.lanes.entry(lane).or_insert_with(Lane::new);
        // An exempt reply has no place to wait behind, and one that is its lane's next may leave at
        // once; anything either of them then waits is delivery, not order.
        let blocked = seq != UNORDERED && seq != entry.next;
        let held = Held {
            ready_at,
            value,
            blocked,
        };
        if seq == UNORDERED {
            entry.exempt.push_back(held);
            self.held += 1;
            self.enrol(lane);
            return;
        }
        // Ordinarily a reply fills the place reserved for it. The fault makes two places trade
        // contents, so this reply leaves in an earlier one's turn and that one leaves in this one's —
        // both still filled, both still released, and the sequencer sees an arrival out of order,
        // which is what its gap detection is for.
        if let Some(at) = entry.swapped.remove(&seq) {
            entry.places.insert(at, Some(held));
        } else if violate && entry.next != seq && entry.places.contains_key(&entry.next) {
            let next = entry.next;
            match entry.places.insert(next, Some(held)) {
                // The place it is stealing was already filled, so the two simply trade: both leave, in
                // each other's turn.
                Some(Some(other)) => {
                    entry.places.insert(seq, Some(other));
                }
                // It was reserved and still empty, so the reply it belongs to has not arrived. Remember
                // where to put it when it does.
                _ => {
                    entry.places.insert(seq, None);
                    entry.swapped.insert(next, seq);
                }
            }
        } else {
            entry.places.insert(seq, Some(held));
        }
        self.held += 1;
        let depth = entry.places.len() + entry.exempt.len();
        self.deepest_lane = self.deepest_lane.max(depth);
        if blocked {
            self.order_held += 1;
        }
        self.enrol(lane);
    }

    fn lane_mut(&mut self, lane: AccountId) -> &mut Lane<T> {
        self.lanes.entry(lane).or_insert_with(Lane::new)
    }

    fn enrol(&mut self, lane: AccountId) {
        let entry = self.lanes.get_mut(&lane).expect("a lane just touched");
        if !entry.in_rotation {
            entry.in_rotation = true;
            self.rotation.push_back(lane);
        }
    }

    /// The earliest a lane's next reply could leave, so a virtual clock can jump to it instead of
    /// crawling there. Only the head of each waiting lane can be the answer.
    pub fn next_due(&self) -> Option<u64> {
        self.rotation
            .iter()
            .filter_map(|lane| {
                let lane = self.lanes.get(lane)?;
                let seq = lane.release_seq()?;
                lane.head(seq).map(|held| held.ready_at)
            })
            .min()
    }

    /// Replies finished but not releasable, because their lane is waiting for an earlier seq. This is
    /// what putting a lane back in order costs: the wait is the depth times a read rather than one
    /// read, which no per-read bound covers. A lane whose turn has simply not come round yet counts,
    /// because that is precisely the reply that is paying.
    pub fn behind_heads(&self) -> usize {
        let releasable = self
            .rotation
            .iter()
            .filter(|lane| {
                self.lanes
                    .get(lane)
                    .is_some_and(|lane| lane.release_seq().is_some())
            })
            .count();
        self.held.saturating_sub(releasable)
    }

    pub fn order_wait(&self) -> OrderWait {
        OrderWait {
            released: self.released,
            held_for_order: self.order_held,
            order_nanos: self.order_nanos,
            order_worst_nanos: self.order_worst_nanos,
            delivery_nanos: self.delivery_nanos,
            deepest_lane: self.deepest_lane,
        }
    }

    /// The next reply due to leave, or nothing. Head of lane only, which is what keeps a lane in order.
    pub fn pop_ready(&mut self, now: u64) -> Option<T> {
        for _ in 0..self.rotation.len() {
            let lane_id = self.rotation.pop_front()?;
            let Some(lane) = self.lanes.get_mut(&lane_id) else {
                continue;
            };
            // Kept in the rotation even when its turn has not come: the lane still holds a reply, and
            // a lane dropped from here would be invisible to whatever reports what ordering costs.
            let Some(seq) = lane.release_seq() else {
                self.rotation.push_back(lane_id);
                continue;
            };
            let ready = lane.head(seq).is_some_and(|held| held.ready_at <= now);
            if !ready {
                self.rotation.push_back(lane_id);
                continue;
            }
            let held = match seq {
                UNORDERED => lane.exempt.pop_front().expect("a due reply"),
                seq => {
                    let held = lane.places.remove(&seq).flatten().expect("a due reply");
                    lane.next = seq + 1;
                    held
                }
            };
            self.held -= 1;
            self.released += 1;
            let waited = now.saturating_sub(held.ready_at);
            if held.blocked {
                self.order_nanos += waited;
                self.order_worst_nanos = self.order_worst_nanos.max(waited);
            } else {
                self.delivery_nanos += waited;
            }
            if lane.places.is_empty() && lane.exempt.is_empty() && lane.swapped.is_empty() {
                lane.in_rotation = false;
            } else {
                self.rotation.push_back(lane_id);
            }
            return Some(held.value);
        }
        None
    }
}

impl<T> Lane<T> {
    fn new() -> Self {
        Self {
            next: 0,
            places: FxHashMap::default(),
            exempt: VecDeque::new(),
            swapped: FxHashMap::default(),
            in_rotation: false,
        }
    }

    /// Which seq this lane may release now: the place it is waiting for, once filled, or an unordered
    /// reply, which keeps no place and never has to wait for one.
    fn release_seq(&self) -> Option<Seq> {
        if !self.exempt.is_empty() {
            return Some(UNORDERED);
        }
        matches!(self.places.get(&self.next), Some(Some(_))).then_some(self.next)
    }

    fn head(&self, seq: Seq) -> Option<&Held<T>> {
        match seq {
            UNORDERED => self.exempt.front(),
            seq => self.places.get(&seq)?.as_ref(),
        }
    }
}

/// What putting a lane back in order cost. Separate from the device's own numbers, because a read that
/// finished in a millisecond and then waited nine for an earlier read on its lane is a speed problem no
/// per-read bound covers.
#[derive(Debug, Clone, Copy, Default)]
pub struct OrderWait {
    pub released: u64,
    /// Replies that arrived behind a place their lane had not filled yet. Only these pay order-wait, and
    /// only these are what exemption would remove.
    pub held_for_order: u64,
    pub order_nanos: u64,
    pub order_worst_nanos: u64,
    /// What replies waited that had nothing ahead of them: this loop's delay and the reactor's
    /// backpressure. Reported beside the other so neither can be read as the other.
    pub delivery_nanos: u64,
    /// The deepest a single lane got, which is the term the per-read speed contract cannot cover.
    pub deepest_lane: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANE: AccountId = AccountId(9);

    /// The property the whole structure exists for: places are reserved in the order the commands
    /// arrived, and work that finishes in another order still leaves in theirs. Releasing in finish
    /// order would be the device's order, and the sequencer would call it a gap.
    #[test]
    fn work_that_finishes_out_of_order_leaves_in_the_order_it_arrived() {
        let mut orderer: Orderer<&str> = Orderer::new(0);
        orderer.expect(LANE, 4);
        orderer.expect(LANE, 5);

        orderer.fill(LANE, 5, 10, "second");
        assert!(
            orderer.pop_ready(1_000).is_none(),
            "the later place left first"
        );

        orderer.fill(LANE, 4, 20, "first");
        assert_eq!(orderer.pop_ready(1_000), Some("first"));
        assert_eq!(orderer.pop_ready(1_000), Some("second"));
        assert_eq!(orderer.pop_ready(1_000), None);
    }

    /// A run starts wherever the sequencer's fence rule makes it start. The engine sees only a
    /// subsequence of a lane's seqs — a hold never travels this path — so waiting for seq one would
    /// wait forever, which is exactly how this was found: two tests hung.
    #[test]
    fn a_run_begins_at_whatever_seq_arrives_after_the_lane_falls_quiet() {
        let mut orderer: Orderer<&str> = Orderer::new(0);
        orderer.expect(LANE, 7);
        orderer.fill(LANE, 7, 0, "seventh");
        assert_eq!(orderer.pop_ready(1), Some("seventh"));

        // The lane fell quiet; the next run starts wherever it starts.
        orderer.expect(LANE, 30);
        orderer.fill(LANE, 30, 0, "thirtieth");
        assert_eq!(orderer.pop_ready(1), Some("thirtieth"));
    }

    /// Order is not the only thing a reply waits for: its own work has to be done. A reply released
    /// before its device finished would be an answer nobody computed.
    #[test]
    fn a_reply_waits_for_its_own_work_as_well_as_its_turn() {
        let mut orderer: Orderer<&str> = Orderer::new(0);
        orderer.expect(LANE, 1);
        orderer.fill(LANE, 1, 500, "first");
        assert!(
            orderer.pop_ready(499).is_none(),
            "released before it was finished"
        );
        assert_eq!(orderer.next_due(), Some(500));
        assert_eq!(orderer.pop_ready(500), Some("first"));
    }

    /// Two lanes have no order between them, so one lane waiting for an earlier place must not hold the
    /// other up. A single queue would.
    #[test]
    fn one_lane_waiting_does_not_hold_another_up() {
        let other = AccountId(10);
        let mut orderer: Orderer<&str> = Orderer::new(0);
        orderer.expect(LANE, 1);
        orderer.expect(LANE, 2);
        orderer.fill(LANE, 2, 0, "blocked");
        orderer.expect(other, 1);
        orderer.fill(other, 1, 0, "free");

        assert_eq!(orderer.pop_ready(1), Some("free"));
        assert_eq!(orderer.pop_ready(1), None);
        // Finished, and waiting for a place that has not been filled. That wait is what ordering costs.
        assert_eq!(orderer.behind_heads(), 1);
    }

    /// The fault, which exists so the sequencer's own detection can be tested: a reply claims the place
    /// the lane is waiting for and leaves in the wrong order on purpose.
    #[test]
    fn the_fault_releases_a_reply_out_of_its_turn() {
        let mut orderer: Orderer<&str> = Orderer::new(1);
        orderer.expect(LANE, 5);
        orderer.expect(LANE, 6);
        orderer.fill(LANE, 6, 0, "out of turn");
        assert_eq!(
            orderer.pop_ready(1),
            Some("out of turn"),
            "the fault did not fire"
        );
    }

    /// An order-exempt reply keeps no place, so it never waits for one — and it does not disturb the
    /// numbering of the replies that do. Several of them on one lane must all survive, which a map
    /// keyed by seq could not manage.
    #[test]
    fn unordered_replies_leave_without_waiting_and_do_not_overwrite_each_other() {
        let mut orderer: Orderer<&str> = Orderer::new(0);
        orderer.expect(LANE, 1);
        orderer.fill(LANE, UNORDERED, 0, "exempt one");
        orderer.fill(LANE, UNORDERED, 0, "exempt two");
        assert_eq!(orderer.pop_ready(1), Some("exempt one"));
        assert_eq!(orderer.pop_ready(1), Some("exempt two"));

        orderer.fill(LANE, 1, 0, "ordered");
        assert_eq!(orderer.pop_ready(1), Some("ordered"));
    }
}
