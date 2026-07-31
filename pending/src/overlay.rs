use ledger_base::ports::OverlayState;
use ledger_base::{Amount, Footprint, FxHashMap, Peak, TxId};

/// The sequencer's own decisions about the holds it has in flight: what it last told the engine each
/// one has left, and what proposed-but-uncommitted resolutions have taken of that. None of it is
/// anywhere else, because the store only learns a decision when its batch commits.
///
/// It holds no copy of a record. A record is what a hold *is* — its accounts, its ledger, its group —
/// and that belongs to the engine, which answers a lookup with it; keeping a second copy here would
/// be the same fact under two owners, and nothing would say which one was true.
pub struct HoldOverlay {
    entries: FxHashMap<TxId, Entry>,
    soft_limit: usize,
    eviction_per_round: usize,
    /// The most entries held at once. Eviction means the current count says nothing about what had to
    /// have room.
    peak: Peak,
}

enum Entry {
    /// Nothing decided yet. It exists so that an answer on its way, and the pins of the requests
    /// waiting for it, have somewhere to land.
    Watched {
        pinned: u32,
    },
    /// The engine looked and the hold is not there. Kept, because asking again would get the same
    /// answer.
    Missing {
        pinned: u32,
    },
    /// A committed removal took the hold away while a request in flight still had it pinned. Answers
    /// the same as `Missing` — the hold is gone — but exists only to carry those pins, so it goes with
    /// the last of them instead of being kept for the next lookup to find.
    Removed {
        pinned: u32,
    },
    Decided(Hold),
}

impl Entry {
    fn pins(&mut self) -> &mut u32 {
        match self {
            Entry::Watched { pinned } | Entry::Missing { pinned } | Entry::Removed { pinned } => {
                pinned
            }
            Entry::Decided(hold) => &mut hold.pinned,
        }
    }

    fn pinned(&self) -> u32 {
        match self {
            Entry::Watched { pinned } | Entry::Missing { pinned } | Entry::Removed { pinned } => {
                *pinned
            }
            Entry::Decided(hold) => hold.pinned,
        }
    }
}

struct Hold {
    /// Requests in flight that will read this hold. Eviction leaves those alone.
    pinned: u32,
    /// What the sequencer last told the engine is left.
    committed_remaining: Amount,
    /// How much of that remainder proposed-but-uncommitted resolutions took. Handed over on
    /// commit, given back on failure.
    reserved: Amount,
    /// Whether an in-flight resolution has already consumed the hold entirely.
    resolved: bool,
}

impl HoldOverlay {
    pub fn new(capacity: usize, soft_limit: usize, eviction_per_round: usize) -> Self {
        Self {
            entries: FxHashMap::with_capacity_and_hasher(capacity, Default::default()),
            soft_limit,
            eviction_per_round,
            peak: Peak::default(),
        }
    }

    pub fn hold_is_missing(&self, hold: TxId) -> bool {
        matches!(
            self.entries.get(&hold),
            Some(Entry::Missing { .. } | Entry::Removed { .. })
        )
    }

    /// Only a place for the pins of the requests waiting on the answer. What is already here must
    /// survive its own lookup: these are the sequencer's decisions, and the answer coming back can
    /// have been in flight across them.
    pub fn begin_lookup(&mut self, hold: TxId) {
        self.entries
            .entry(hold)
            .or_insert(Entry::Watched { pinned: 0 });
        self.peak.saw(self.entries.len());
    }

    pub fn pin(&mut self, hold: TxId) {
        if let Some(entry) = self.entries.get_mut(&hold) {
            *entry.pins() += 1;
        }
    }

    pub fn unpin(&mut self, hold: TxId) {
        let Some(entry) = self.entries.get_mut(&hold) else {
            return;
        };
        let pins = entry.pins();
        // An entry a committed removal took away is simply gone, but while it is here every unpin
        // must answer a pin: a leak would keep it forever, a double unpin would let eviction drop it
        // while a request is still coming for it.
        debug_assert!(*pins > 0, "unpin without a pin");
        *pins = pins.saturating_sub(1);
        // A removal's marker exists only to carry pins, so it goes with the last of them. Left to
        // housekeeping instead, a workload that resolves every hold would leave a marker per hold
        // until the soft limit was reached. A `Missing` from a lookup is a different thing and stays:
        // asking again would get the same answer.
        let retire = *pins == 0 && matches!(entry, Entry::Removed { .. });
        if retire {
            self.entries.remove(&hold);
        }
    }

    /// The answer's remainder, or that the hold is not there. Either way it is dropped if a decision
    /// has been taken since: the sequencer takes one the moment it decides, and this answer left
    /// before that — so "not there" can be about a hold that has since been created, and a remainder
    /// can be the one before a settle that has already committed.
    pub fn admit_lookup(&mut self, hold: TxId, remaining: Option<Amount>) {
        if self.decided(hold).is_some() {
            return;
        }
        match remaining {
            Some(remaining) => self.decide(hold, remaining),
            None => {
                let pinned = self.entries.get_mut(&hold).map_or(0, |entry| *entry.pins());
                self.keep(hold, Entry::Missing { pinned });
            }
        }
    }

    /// A hold the engine has just been told to create has all of itself left. It gets an entry only if
    /// something is already here to correct — an answer of "not there" from before it existed, or a
    /// lookup already in flight. Creating one otherwise would say nothing the record does not: no
    /// decision has been taken about a hold nobody has resolved yet, and the entry would then live as
    /// long as the hold rather than as long as a request, which is the difference between this being
    /// bounded by work in flight and bounded by holds outstanding. Measured with
    /// `ledgerfio run --workload hold-settle --resolve-after 900000`: a hundred megabytes of entries
    /// that answered nothing.
    pub fn created(&mut self, hold: TxId, amount: Amount) {
        if self.entries.contains_key(&hold) {
            self.decide(hold, amount);
        }
    }

    pub fn overlay(&self, hold: TxId) -> OverlayState {
        match self.decided(hold) {
            Some(decided) => OverlayState {
                remaining: Some(decided.committed_remaining),
                taken: decided.reserved,
                resolved: decided.resolved,
            },
            None => OverlayState::default(),
        }
    }

    fn decide(&mut self, hold: TxId, remaining: Amount) {
        let pinned = self.entries.get_mut(&hold).map_or(0, |entry| *entry.pins());
        self.keep(
            hold,
            Entry::Decided(Hold {
                pinned,
                committed_remaining: remaining,
                reserved: 0,
                resolved: false,
            }),
        );
    }

    pub fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        if let Some(decided) = self.decided_mut(hold) {
            decided.reserved += amount;
            decided.resolved |= resolves;
        }
    }

    /// The batch committed, so the reservation is no longer speculative. What the hold has left
    /// follows from the write the engine is told about, not from here — one value, one owner. A
    /// reservation that was never taken cannot be given back: a resolution judged inside the chain
    /// that created the hold had nothing here to reserve.
    pub fn release_reservation(&mut self, hold: TxId, amount: Amount) {
        if let Some(decided) = self.decided_mut(hold) {
            decided.reserved = (decided.reserved - amount).max(0);
        }
    }

    /// The engine has been told the hold has this much left.
    pub fn note_remaining(&mut self, hold: TxId, remaining: Amount) {
        if let Some(decided) = self.decided_mut(hold) {
            decided.committed_remaining = remaining;
        }
    }

    /// A committed removal takes the hold away. While a request that pinned it is still in flight the
    /// entry stays, as `Missing`: dropping it would drop that request's pin with it, and the next
    /// lookup would recreate the entry at zero pins — so the in-flight request's unpin would come off
    /// whatever pin another request had since taken, and eviction could then take an entry still to be
    /// read. `maintain` already refuses to drop a pinned entry; this is that invariant on the other
    /// path. `Missing` is also the truthful state: the hold is gone, and a request still coming for it
    /// should be told so rather than pay a lookup to be told the same.
    pub fn forget(&mut self, hold: TxId) {
        let Some(entry) = self.entries.get_mut(&hold) else {
            return;
        };
        match entry.pinned() {
            0 => {
                self.entries.remove(&hold);
            }
            pinned => *entry = Entry::Removed { pinned },
        }
    }

    pub fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        if let Some(decided) = self.decided_mut(hold) {
            decided.reserved -= amount;
            if resolves {
                decided.resolved = false;
            }
        }
    }

    /// Idle entries can be dropped; a later request looks them up again.
    pub fn maintain(&mut self) -> usize {
        if self.entries.len() <= self.soft_limit {
            return 0;
        }
        let budget = self.eviction_per_round;
        let mut evicted = 0;
        self.entries.retain(|_, entry| {
            if evicted >= budget {
                return true;
            }
            if entry.pinned() > 0 {
                return true;
            }
            // A decision nothing is waiting on can go: what the hold has left is in the engine's own
            // record by then, and a later request looks it up again. A negative answer can go any
            // time, and so can a placeholder nothing pinned — the answer it was waiting for either
            // arrived or belongs to a request that is gone. A removal's marker is unreachable here: it
            // only exists while pinned, and this arm is past the pinned check.
            let idle = match entry {
                Entry::Decided(decided) => decided.reserved == 0 && !decided.resolved,
                Entry::Watched { .. } | Entry::Missing { .. } | Entry::Removed { .. } => true,
            };
            if idle {
                evicted += 1;
                return false;
            }
            true
        });
        evicted
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// What the overlay is holding. The peak is the number that sizes it, since eviction means the
    /// count at the end is whatever the run happened to leave behind.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        let mut sized = Footprint::new();
        sized.hash_table::<TxId, Entry>(
            "overlay entries",
            self.entries.len(),
            self.entries.capacity(),
            self.peak.entries(),
        );
        // Its ceiling is the engine's own eviction policy, not a queue anyone else chose, so a fill
        // ratio here would warn about the engine doing its job.
        for part in sized.parts() {
            footprint.other(part.name, part.entries, part.peak_entries, 0, part.bytes);
        }
        footprint
    }

    /// Every insertion goes through here, so the peak is recorded in one place and cannot drift from
    /// the map it describes.
    fn keep(&mut self, hold: TxId, entry: Entry) {
        self.entries.insert(hold, entry);
        self.peak.saw(self.entries.len());
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn decided(&self, hold: TxId) -> Option<&Hold> {
        match self.entries.get(&hold)? {
            Entry::Decided(decided) => Some(decided),
            _ => None,
        }
    }

    fn decided_mut(&mut self, hold: TxId) -> Option<&mut Hold> {
        match self.entries.get_mut(&hold)? {
            Entry::Decided(decided) => Some(decided),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::TxId;

    use super::HoldOverlay;

    const HOLD: TxId = TxId(7);

    /// A pin says a request in flight is still going to read this entry, and eviction honours that.
    /// A committed removal used to drop the entry regardless: the next lookup then recreated it at
    /// zero pins, so the in-flight request's unpin came off whatever pin another request had taken —
    /// and eviction could take an entry still to be read. Nothing in the ledger's own audit sees that,
    /// which is why it survived until a debug build was run under fault injection.
    #[test]
    fn a_removal_keeps_a_pinned_entry_and_gives_it_up_once_unpinned() {
        // A soft limit of zero, so housekeeping evicts whatever it is allowed to.
        let mut overlay = HoldOverlay::new(4, 0, 4);
        // An entry exists because a request looked the hold up, which is the only thing that makes one.
        overlay.admit_lookup(HOLD, Some(100));
        overlay.pin(HOLD);

        overlay.forget(HOLD);

        // The hold is gone, so the truthful answer is that it is not there — but the entry stays,
        // because the pin does.
        assert!(overlay.hold_is_missing(HOLD));
        overlay.maintain();
        assert!(
            overlay.hold_is_missing(HOLD),
            "a pinned entry may not be evicted"
        );

        // The marker exists only to carry the pin, so it goes with it rather than waiting for
        // housekeeping: a workload that resolves every hold would otherwise leave one per hold.
        overlay.unpin(HOLD);
        assert!(!overlay.hold_is_missing(HOLD), "unpinned, it goes at once");
        assert!(overlay.is_empty());
    }

    /// The ordinary case still costs nothing: with nobody reading it, a removal is a removal.
    #[test]
    fn a_removal_with_no_pin_takes_the_entry_away_at_once() {
        let mut overlay = HoldOverlay::new(4, 1 << 10, 4);
        overlay.admit_lookup(HOLD, Some(100));
        overlay.forget(HOLD);
        assert!(!overlay.hold_is_missing(HOLD));
        assert!(overlay.is_empty());
    }

    /// An answer is a reading of the store taken when the lookup was served; a decision is taken the
    /// moment the sequencer makes it. So when they disagree the decision is the newer of the two, and
    /// an answer that crossed it may not be believed — in either direction. Both cases are the same
    /// rule, which is why they are one test.
    #[test]
    fn a_decision_outranks_an_answer_that_was_already_in_flight() {
        let mut overlay = HoldOverlay::new(4, 1 << 10, 4);

        // A settle asks about a hold that does not exist yet, and the hold is created before the
        // answer comes back. "Not there" would reject every later resolution of a hold that exists.
        overlay.begin_lookup(HOLD);
        overlay.created(HOLD, 100);
        overlay.admit_lookup(HOLD, None);
        assert!(
            !overlay.hold_is_missing(HOLD),
            "the hold was created while the answer was in flight"
        );

        // The other direction: a settle commits, so the sequencer knows the hold is down to 40, and
        // then an answer taken before that write arrives saying 100.
        overlay.note_remaining(HOLD, 40);
        overlay.admit_lookup(HOLD, Some(100));
        assert_eq!(
            overlay.overlay(HOLD).remaining,
            Some(40),
            "the older reading was dropped"
        );
    }
}
