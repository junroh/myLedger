use ledger_base::ports::{HoldData, HoldView, OverlayState};
use ledger_base::{Amount, Footprint, FxHashMap, Peak, TxId};

/// A copy of what the store last confirmed about a hold, plus the reservations against it that no
/// batch has committed yet. The copy is why judging needs no round trip; the reservations exist
/// nowhere else, because the store only learns a decision when its batch commits.
///
/// It sits on the reactor's thread since the judge cannot continue without an answer, but the state
/// and the eviction policy are still the engine's.
pub struct HoldOverlay {
    entries: FxHashMap<TxId, Entry>,
    soft_limit: usize,
    eviction_per_round: usize,
    /// The most entries held at once. Eviction means the current count says nothing about what the
    /// engine had to have room for.
    peak: Peak,
}

enum Entry {
    /// A lookup has been sent and the answer has not arrived.
    LookupSent { pinned: u32 },
    /// The engine looked and the hold is not there. Kept, because asking again would get the same
    /// answer.
    Missing { pinned: u32 },
    /// A committed removal took the hold away while a request in flight still had it pinned. Answers
    /// the same as `Missing` — the hold is gone — but exists only to carry those pins, so it goes with
    /// the last of them instead of being kept for the next lookup to find.
    Removed { pinned: u32 },
    Live(Hold),
}

impl Entry {
    fn pins(&mut self) -> &mut u32 {
        match self {
            Entry::LookupSent { pinned } | Entry::Missing { pinned } | Entry::Removed { pinned } => {
                pinned
            }
            Entry::Live(live) => &mut live.pinned,
        }
    }

    fn pinned(&self) -> u32 {
        match self {
            Entry::LookupSent { pinned }
            | Entry::Missing { pinned }
            | Entry::Removed { pinned } => *pinned,
            Entry::Live(live) => live.pinned,
        }
    }
}

struct Hold {
    data: HoldData,
    /// Requests in flight that will read this hold. Eviction leaves those alone.
    pinned: u32,
    /// What the store last confirmed is left.
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

    pub fn state(&self, hold: TxId) -> OverlayState {
        match self.entries.get(&hold) {
            None => OverlayState::Absent,
            Some(Entry::LookupSent { .. }) => OverlayState::LookupSent,
            Some(Entry::Missing { .. } | Entry::Removed { .. }) => OverlayState::Missing,
            Some(Entry::Live(_)) => OverlayState::Ready,
        }
    }

    pub fn begin_lookup(&mut self, hold: TxId) {
        let pinned = self.entries.get_mut(&hold).map_or(0, |entry| *entry.pins());
        self.keep(hold, Entry::LookupSent { pinned });
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

    pub fn admit_lookup(&mut self, hold: TxId, found: Option<HoldData>) {
        match found {
            Some(data) => self.admit(hold, data),
            None => {
                let pinned = self.entries.get_mut(&hold).map_or(0, |entry| *entry.pins());
                self.keep(hold, Entry::Missing { pinned });
            }
        }
    }

    /// A hold the engine has just been told to create is known exactly, so the overlay starts from
    /// it instead of paying a lookup to be told what was already decided.
    pub fn admit(&mut self, hold: TxId, data: HoldData) {
        let remaining = data.remaining;
        let pinned = self.entries.get_mut(&hold).map_or(0, |entry| *entry.pins());
        self.keep(
            hold,
            Entry::Live(Hold {
                data,
                pinned,
                committed_remaining: remaining,
                reserved: 0,
                resolved: false,
            }),
        );
    }

    pub fn view(&self, hold: TxId) -> Option<HoldView> {
        let live = self.live(hold)?;
        Some(HoldView {
            debit_account: live.data.debit_account,
            credit_account: live.data.credit_account,
            ledger: live.data.ledger,
            budget: live.data.budget,
            budget_members: live.data.budget_members,
            budget_remaining: live.data.budget_remaining,
            remaining: live.committed_remaining - live.reserved,
            resolved: live.resolved,
        })
    }

    pub fn reserve(&mut self, hold: TxId, amount: Amount, resolves: bool) {
        if let Some(live) = self.live_mut(hold) {
            live.reserved += amount;
            live.resolved |= resolves;
        }
    }

    /// The batch committed, so the reservation is no longer speculative. What the hold has left
    /// follows from the write the engine is told about, not from here — one value, one owner. A
    /// reservation that was never taken cannot be given back: a resolution judged inside the chain
    /// that created the hold had nothing here to reserve.
    pub fn release_reservation(&mut self, hold: TxId, amount: Amount) {
        if let Some(live) = self.live_mut(hold) {
            live.reserved = (live.reserved - amount).max(0);
        }
    }

    /// The store has been told the hold has this much left.
    pub fn note_remaining(&mut self, hold: TxId, remaining: Amount) {
        if let Some(live) = self.live_mut(hold) {
            live.committed_remaining = remaining;
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
        if let Some(live) = self.live_mut(hold) {
            live.reserved -= amount;
            if resolves {
                live.resolved = false;
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
            let idle = match entry {
                Entry::Live(live) => live.reserved == 0 && !live.resolved,
                // An answer that has not arrived is not idle; a negative one can go any time. A
                // removal's marker is unreachable here — it only exists while pinned, and this arm is
                // past the pinned check — but it is idle by the same argument.
                Entry::LookupSent { .. } => false,
                Entry::Missing { .. } | Entry::Removed { .. } => true,
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

    fn live(&self, hold: TxId) -> Option<&Hold> {
        match self.entries.get(&hold)? {
            Entry::Live(live) => Some(live),
            _ => None,
        }
    }

    fn live_mut(&mut self, hold: TxId) -> Option<&mut Hold> {
        match self.entries.get_mut(&hold)? {
            Entry::Live(live) => Some(live),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::ports::{HoldData, OverlayState};
    use ledger_base::TxId;

    use super::HoldOverlay;

    const HOLD: TxId = TxId(7);

    fn hold() -> HoldData {
        HoldData {
            debit_account: ledger_base::AccountId(1),
            credit_account: ledger_base::AccountId(2),
            amount: 100,
            remaining: 100,
            ledger: 1,
            budget: Default::default(),
            budget_members: 0,
            budget_remaining: 0,
        }
    }

    /// A pin says a request in flight is still going to read this entry, and eviction honours that.
    /// A committed removal used to drop the entry regardless: the next lookup then recreated it at
    /// zero pins, so the in-flight request's unpin came off whatever pin another request had taken —
    /// and eviction could take an entry still to be read. Nothing in the ledger's own audit sees that,
    /// which is why it survived until a debug build was run under fault injection.
    #[test]
    fn a_removal_keeps_a_pinned_entry_and_gives_it_up_once_unpinned() {
        // A soft limit of zero, so housekeeping evicts whatever it is allowed to.
        let mut overlay = HoldOverlay::new(4, 0, 4);
        overlay.admit(HOLD, hold());
        overlay.pin(HOLD);

        overlay.forget(HOLD);

        // The hold is gone, so the truthful answer is that it is not there — but the entry stays,
        // because the pin does.
        assert_eq!(overlay.state(HOLD), OverlayState::Missing);
        overlay.maintain();
        assert_eq!(overlay.state(HOLD), OverlayState::Missing, "a pinned entry may not be evicted");

        // The marker exists only to carry the pin, so it goes with it rather than waiting for
        // housekeeping: a workload that resolves every hold would otherwise leave one per hold.
        overlay.unpin(HOLD);
        assert_eq!(overlay.state(HOLD), OverlayState::Absent, "unpinned, it goes at once");
        assert!(overlay.is_empty());
    }

    /// The ordinary case still costs nothing: with nobody reading it, a removal is a removal.
    #[test]
    fn a_removal_with_no_pin_takes_the_entry_away_at_once() {
        let mut overlay = HoldOverlay::new(4, 1 << 10, 4);
        overlay.admit(HOLD, hold());
        overlay.forget(HOLD);
        assert_eq!(overlay.state(HOLD), OverlayState::Absent);
        assert!(overlay.is_empty());
    }
}
