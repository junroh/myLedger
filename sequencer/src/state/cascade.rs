use crate::state::pipeline::SlotId;

/// Slots a chain gate has just freed and which are judgeable now, waiting for the loop that freed them.
///
/// A chain being judged opens the gate its lane was holding, which frees every request queued behind it,
/// and one of those may complete the next chain, whose gate frees its own queue. So the run is as long as
/// the backlog of chains on one lane — a client's doing, not the reactor's, and nothing here bounds it.
/// Walking it by nested call made the stack that bound: `ledgerfio run --workload linked` reached a depth
/// of fourteen hundred and aborted the reactor thread. Rule 12 is about queues, and this is the same rule
/// applied to the one backlog that was not a queue.
///
/// A stack, and pushed in reverse, so what comes off it is exactly the order the nested calls took.
/// Judging is what fixes a lane's seq order, so a different order here would be a contract-1 violation of
/// our own making rather than a component's.
pub struct Cascade {
    ready: Vec<SlotId>,
    /// Set while a loop is draining, so a chain judged inside one adds to it rather than starting another.
    running: bool,
}

impl Cascade {
    /// Sized for the slot pool, which is the ceiling: a slot appears here only while it is judgeable, and
    /// judging it frees it. Reserved once so a cascade allocates nothing.
    pub fn with_capacity(slots: usize) -> Self {
        Self {
            ready: Vec::with_capacity(slots),
            running: false,
        }
    }

    pub fn push(&mut self, slot: SlotId) {
        self.ready.push(slot);
    }

    /// True when the caller now owns the loop, false when an outer call already does.
    pub fn enter(&mut self) -> bool {
        let mine = !self.running;
        self.running = true;
        mine
    }

    pub fn next(&mut self) -> Option<SlotId> {
        self.ready.pop()
    }

    pub fn leave(&mut self) {
        self.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One loop, whoever arrives first. The second caller in must be told it does not own the loop, or it
    /// starts its own — which is the nesting this exists to remove, and the deeper the backlog the more of
    /// it there would be.
    #[test]
    fn only_the_first_caller_in_owns_the_loop() {
        let mut cascade = Cascade::with_capacity(4);
        assert!(cascade.enter(), "the first caller owns the loop");
        assert!(
            !cascade.enter(),
            "a nested caller must not start a second loop"
        );
        assert!(!cascade.enter());

        cascade.leave();
        assert!(cascade.enter(), "the next cascade is a new loop");
    }

    /// Handed back in the order they were pushed, which is what keeps judging in a lane's seq order: the
    /// stack is walked from its top, so whoever fills it pushes in reverse.
    #[test]
    fn slots_come_off_in_the_order_they_were_queued() {
        let mut cascade = Cascade::with_capacity(4);
        for slot in [3, 2, 1] {
            cascade.push(slot);
        }
        assert_eq!(
            [
                cascade.next(),
                cascade.next(),
                cascade.next(),
                cascade.next()
            ],
            [Some(1), Some(2), Some(3), None]
        );
    }
}
