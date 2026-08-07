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

/// What one overlay entry costs, priced as the hash table it is. Larger than the index's slot, which
/// surprises: a live hold costs the index eight bytes and the overlay forty-nine, because the overlay
/// keeps what was decided about it and the index keeps only where it is.
pub const ENTRY_BUCKET_BYTES: usize = ledger_base::bucket_bytes::<TxId, Entry>();

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
    /// A committed removal took the hold away. Answers the same as `Missing` — the hold is gone — and
    /// it is the **only** thing that says so until the engine has applied the removal: the sequencer
    /// erases this the moment it hands the write over, and the engine clears its index a queue later.
    /// A lookup answered inside that gap carries the hold as it was, with its remainder intact, and
    /// without this marker it would decide a live entry from that answer and resolve the hold twice.
    ///
    /// So it lives until the engine has caught up. `safe_after` is the number of applies this side had
    /// sent when the removal went; once the engine reports at least that many, its index no longer
    /// points at the hold and a lookup misses on its own.
    Removed {
        pinned: u32,
        safe_after: u64,
    },
    Decided(Hold),
}

impl Entry {
    fn pins(&mut self) -> &mut u32 {
        match self {
            Entry::Watched { pinned }
            | Entry::Missing { pinned }
            | Entry::Removed { pinned, .. } => pinned,
            Entry::Decided(hold) => &mut hold.pinned,
        }
    }

    fn pinned(&self) -> u32 {
        match self {
            Entry::Watched { pinned }
            | Entry::Missing { pinned }
            | Entry::Removed { pinned, .. } => *pinned,
            Entry::Decided(hold) => hold.pinned,
        }
    }
}

/// What the overlay knows about one hold, which is a smaller question than which entry it is holding.
/// `Gone` covers both ways a hold can be absent — the engine said so, or a committed removal took it
/// and has not been applied yet — because nothing outside this file needs to tell those apart.
enum Known<'a> {
    Nothing,
    /// A lookup is out and no answer has come back.
    Awaiting,
    Gone,
    Live(&'a Hold),
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

    /// What is known about a hold, decided **once**. Every reader below derives from this rather than
    /// reading an entry and deciding for itself.
    ///
    /// That is not tidiness. `Removed` already existed when its lifetime changed — it used to last as
    /// long as a pin, and now it lasts until the engine has applied the removal — and because three
    /// readers each matched on the entry themselves, three of them had to change and a missed one was
    /// silent. No variant was added, so no exhaustive match would have caught it. One decode point is
    /// what a compiler cannot give here.
    fn known(&self, hold: TxId) -> Known<'_> {
        match self.entries.get(&hold) {
            None => Known::Nothing,
            Some(Entry::Watched { .. }) => Known::Awaiting,
            Some(Entry::Missing { .. } | Entry::Removed { .. }) => Known::Gone,
            Some(Entry::Decided(decided)) => Known::Live(decided),
        }
    }

    pub fn hold_is_missing(&self, hold: TxId) -> bool {
        matches!(self.known(hold), Known::Gone)
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
        // A removal's marker used to go with the last pin, on the reading that it existed only to carry
        // them. It does not: until the engine has applied the removal, it is the only thing that says
        // the hold is gone, and the lookup it has to answer for may not have started. Housekeeping
        // retires it once the engine has caught up — see `forget` and `maintain`.
    }

    /// The answer's remainder, or that the hold is not there. Either way it is dropped if a decision
    /// has been taken since: the sequencer takes one the moment it decides, and this answer left
    /// before that — so "not there" can be about a hold that has since been created, and a remainder
    /// can be the one before a settle that has already committed.
    pub fn admit_lookup(&mut self, hold: TxId, remaining: Option<Amount>) {
        // Anything already known here was decided by the sequencer, and this answer left before that.
        // A removal counts: believing an answer that crossed one was the defect, because the answer
        // still carries the hold with its whole remainder and deciding a live hold from it resolves the
        // hold a second time.
        if matches!(self.known(hold), Known::Live(_) | Known::Gone) {
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
        match self.known(hold) {
            Known::Live(decided) => OverlayState {
                remaining: Some(decided.committed_remaining),
                taken: decided.reserved,
                resolved: decided.resolved,
            },
            // The caller is composing a view from a record a lookup brought back, and where the hold is
            // gone that record can still show it alive with its whole remainder — the engine clears its
            // index a queue after the sequencer hands the removal over. So the answer comes from here.
            Known::Gone => OverlayState {
                remaining: Some(0),
                taken: 0,
                resolved: true,
            },
            Known::Nothing | Known::Awaiting => OverlayState::default(),
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

    /// A committed removal takes the hold away, and this is where that becomes known. The marker is
    /// left **always**, not only while something has the hold pinned: pins are about a request still
    /// coming for the answer, and this is about the engine not having applied the removal yet. Those
    /// are different questions, and answering the second with the first is what let a hold be resolved
    /// twice — the marker went at hand-over, the engine's index cleared a queue later, and a lookup
    /// answered in between said the hold was alive with its whole remainder.
    ///
    /// An entry is made even where there was none, for the same reason: the lookup that will be told
    /// the wrong thing may not have started yet.
    pub fn forget(&mut self, hold: TxId, safe_after: u64) {
        let pinned = self.entries.get_mut(&hold).map_or(0, |entry| *entry.pins());
        self.keep(hold, Entry::Removed { pinned, safe_after });
    }

    pub fn compensate(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        if let Some(decided) = self.decided_mut(hold) {
            decided.reserved -= amount;
            if resolves {
                decided.resolved = false;
            }
        }
    }

    /// Idle entries can be dropped; a later request looks them up again. `applied` is what the engine
    /// says it has got through, which is the one thing that makes a removal's marker droppable.
    pub fn maintain(&mut self, applied: u64) -> usize {
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
            // arrived or belongs to a request that is gone. A removal's marker is the exception, and
            // the only entry here whose life is not about pins: it may go once the engine has applied
            // the removal it stands for, and not before, or a lookup already in flight can still be
            // answered from a record the engine has not cleared.
            let idle = match entry {
                Entry::Decided(decided) => decided.reserved == 0 && !decided.resolved,
                Entry::Removed { safe_after, .. } => applied >= *safe_after,
                Entry::Watched { .. } | Entry::Missing { .. } => true,
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
            "pending overlay",
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
    fn a_removal_is_remembered_until_the_engine_has_applied_it() {
        // A soft limit of zero, so housekeeping evicts whatever it is allowed to.
        let mut overlay = HoldOverlay::new(4, 0, 4);
        overlay.admit_lookup(HOLD, Some(100));
        overlay.pin(HOLD);

        // The tenth apply handed over is this removal.
        overlay.forget(HOLD, 10);

        assert!(overlay.hold_is_missing(HOLD));
        overlay.maintain(9);
        assert!(
            overlay.hold_is_missing(HOLD),
            "a pinned entry may not be evicted"
        );

        // Unpinning is not what makes it droppable. The engine is still one apply short of this
        // removal, so a lookup answered now would carry the hold as it was — which is the whole reason
        // the marker exists.
        overlay.unpin(HOLD);
        overlay.maintain(9);
        assert!(
            overlay.hold_is_missing(HOLD),
            "the marker went before the engine had applied the removal"
        );

        // Now the engine has it, so its index no longer points at the hold and a lookup misses on its
        // own. The marker has nothing left to say.
        overlay.maintain(10);
        assert!(!overlay.hold_is_missing(HOLD));
        assert!(overlay.is_empty());
    }

    /// A removal with nothing reading it is remembered too, and that is the case the ledger got wrong:
    /// the marker was dropped at once, the engine cleared its index a queue later, and a lookup that
    /// started in between decided a live hold from an answer taken before the removal — resolving it a
    /// second time and taking the money twice.
    #[test]
    fn a_removal_with_no_pin_is_remembered_too() {
        let mut overlay = HoldOverlay::new(4, 0, 4);
        overlay.admit_lookup(HOLD, Some(100));

        overlay.forget(HOLD, 3);
        assert!(overlay.hold_is_missing(HOLD), "dropped with nobody reading");

        // The answer that crosses the removal. Without the marker this decides a hold with 100 left.
        overlay.admit_lookup(HOLD, Some(100));
        assert!(
            overlay.hold_is_missing(HOLD),
            "an answer from before the removal brought the hold back"
        );

        overlay.maintain(3);
        assert!(!overlay.hold_is_missing(HOLD));
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
