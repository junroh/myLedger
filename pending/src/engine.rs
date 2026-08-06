use ledger_base::ports::{ApplyIndex, HoldData, PendingEffect};
use ledger_base::{Amount, BudgetGroup, FxHashMap, MapGauge, Transfer, TransferFlags, TxId};

use crate::block::{DurableStore, LogTraffic, MemoryStore, RecordAddr, RecordLog, SEGMENTS};
use crate::index::{Candidates, HoldTable};
use crate::snapshot::SnapshotWriter;

/// What the pending engine keeps: every hold it was told to create, and the budget groups those
/// holds belong to. It writes down committed decisions and judges nothing — which is why a write
/// that names a hold the store does not have is dropped rather than refused.
#[derive(Default)]
pub struct PendingEngine {
    /// Where each hold is.
    index: HoldTable,
    /// What each hold is. Append-only, so a hold whose remainder changed is written again and the
    /// index repointed rather than a block rewritten in place.
    records: RecordLog,
    budgets: FxHashMap<BudgetGroup, BudgetState>,
    /// Reused by every compaction, so flushing a block allocates nothing.
    survivors: Vec<(TxId, HoldData, RecordAddr)>,
    /// Lookups waiting on the store, with the rest of their candidate walk.
    fetches: FxHashMap<u64, Fetch>,
    /// The day whose records are being written. Not stored anywhere durable: a segment's number *is* its
    /// day modulo the segments available, and only a lifetime's worth is ever live, so the day is
    /// recoverable from the number alone.
    today: u64,
    /// Days a record may live, as the caller last declared it. Kept beside the day rather than passed to
    /// every call, because it is what decides whether the sweep's day has run out yet.
    lifetime_days: u64,
    sweep: Sweep,
    /// Blocks of expiring days the sweep has read. The sweep's whole cost, and now a bounded one: a round
    /// reads as many blocks as it was asked for, and the records in them are that day's own. Counted so a
    /// run can say what expiry cost it rather than inferring it — the number this replaces was index slots
    /// walked, which nothing bounded.
    swept_blocks: u64,
    /// Expiry voids handed over and not yet landed, with the address each was built from. One slice at a
    /// time, because the sweep is not asked for more until the last is handed over — so this is bounded by
    /// `expiry_blocks_per_round` times `RECORDS_PER_BLOCK`, not by the size of a day.
    ///
    /// It is the retry, and it exists because there was none. A void the sequencer declined or the judge
    /// refused used to be recoverable only by walking the day again, which the sweep would not do until the
    /// day's live count moved — so a declined void that was the last of its day left the day unfinished for
    /// ever, and with it every later day: deletion is strictly ordered and one stuck hold stops all of it.
    /// Four comments claimed "the sweep offers it again" and none of them was true in that case.
    ///
    /// **What it costs, measured:** one extra index probe per void. Retiring an entry asks whether the index
    /// still points at its address, so a round re-probes the slice it is holding — about thirty percent on
    /// top of the walk at the largest size the bench covers (19ns a void to 27ns). The alternative was to
    /// match a removal against this list as it is applied, and that puts the cost on the path that applies
    /// committed decisions in order. Background work is the right place to pay, so it pays there.
    outstanding: Vec<(RecordAddr, Transfer)>,
    overflowed: u64,
    /// The log position of the last batch whose effects reached this engine. Not the same thing as the
    /// count below: that says how many, this says *where*, and only the second is a position a snapshot can
    /// resume from. See `ApplyIndex`.
    applied_through: ApplyIndex,
    /// Committed decisions applied, counted so an answer can say which of them it reflects. The
    /// sequencer knows how many it had sent when it asked, and an answer from before one of them is a
    /// component that has reordered its own queue — which is the check that replaces the lane's order
    /// for a request that keeps no place in it.
    applied: u64,
}

/// Where a lookup has got to: which key it is for, the addresses its fingerprint matched, and how many
/// of them have been ruled out.
struct Fetch {
    key: TxId,
    candidates: Candidates,
    next: usize,
}

/// What starting a lookup did. `Busy` is the store refusing another read, which is backpressure — the
/// caller keeps the command and asks again.
#[derive(Debug, Clone, Copy)]
pub enum Started {
    Answered(Option<HoldData>),
    Fetching,
    Busy,
}

/// Where the expiry sweep has got to. Nothing here is durable: a sweep interrupted by a restart starts
/// again, which costs a walk and loses nothing — the index is the authority on what survived.
///
/// One cursor, not one per day. The oldest day that is not finished is the only one worked on, and it is
/// finished when the index has no entries left in it — which is the same moment its blocks can be handed
/// back. Days therefore cannot be skipped: an earlier version restarted the walk at whatever had just
/// expired, so a day still being walked when the next arrived was abandoned and its holds were never
/// released at all. Falling behind here is late, which costs space rather than correctness.
#[derive(Debug, Clone, Copy, Default)]
struct Sweep {
    /// The oldest day whose holds are not all released and whose blocks are not handed back. `None` until
    /// the engine has been told what day it is: a fresh one has no history, and a cursor starting at day
    /// zero would walk days that predate it — and step over the day its own records are in on the way.
    day: Option<u64>,
    /// Which of the expiring day's blocks the walk has reached. Blocks of that day, not slots of the index:
    /// a day's survivors are recorded in its own blocks, so the walk costs what that day wrote instead of
    /// what every day did. The index scan this replaces cost 2.2 seconds a pass at the design's table, on
    /// this same thread, ahead of the lookups it would otherwise be answering.
    at_block: u64,
}

/// Whether an effect is arriving for the first time or again.
///
/// Replay is a mode rather than a flag on the effect because it changes exactly one thing, and that thing
/// costs a read. A `Create` for a key the index already holds has to become a repoint, and telling "this
/// key is already here" from "another key with the same fingerprint" means reading a record — a slot
/// holds a fingerprint, not a key. On the path that applies committed decisions in order that read is an
/// IO nothing can hide, and §11's claim that the apply path reads nothing is a property worth keeping.
/// It is also a read for a case that path can never meet: a client cannot create one hold twice, because
/// idem refuses the resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    First,
    /// Again, with the position the group totals already reflect — see `PendingEngine::replay`.
    Again(ApplyIndex),
}

/// A hold the index could not take, on its way out of the engine. Named rather than returned as a bare
/// `TxId` because what the caller has to do with it is not retry — there is nowhere for the hold to go,
/// the table was sized for a declared maximum and that maximum has been passed — but tell the sequencer,
/// which stops applying.
#[derive(Debug, Clone, Copy)]
pub struct NotStored {
    pub hold: TxId,
}

/// The group as the store knows it. Membership is why the sequencer can tell a partial
/// resolution from a complete one.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetState {
    members: u32,
    remaining: Amount,
}

impl BudgetState {
    pub fn members(&self) -> u32 {
        self.members
    }

    pub fn remaining(&self) -> Amount {
        self.remaining
    }

    /// Rebuilt from a snapshot. Not `new`: this is the only way one is made from outside, and the name says
    /// that the numbers came from a stream rather than from applies the engine counted.
    pub fn restored(members: u32, remaining: Amount) -> Self {
        Self { members, remaining }
    }
}

impl PendingEngine {
    /// Sized from what the configuration declared. The table does not grow, so this is the only place its
    /// size is decided.
    pub fn sized(
        slots: usize,
        flush_blocks: usize,
        resident_blocks: usize,
        store: Box<dyn DurableStore>,
    ) -> Self {
        Self {
            index: HoldTable::with_slots(slots),
            records: RecordLog::new(store, flush_blocks, resident_blocks),
            ..Self::default()
        }
    }

    /// Windows of its own, for a test that wants the buffer small enough to see it compact, or
    /// residency small enough to see a record leave memory.
    pub fn with_windows(flush_blocks: usize, resident_blocks: usize) -> Self {
        Self {
            records: RecordLog::new(
                Box::new(MemoryStore::default()),
                flush_blocks,
                resident_blocks,
            ),
            ..Self::default()
        }
    }

    /// Answers with the hold and, when it belongs to one, its group as a whole. The group rides
    /// along because a resolution cannot be checked for coverage without both.
    ///
    /// Takes `&mut self` because reading a record may reach past the block being filled, and the
    /// buffer it is read into belongs to the engine — a lookup that allocated one would allocate per
    /// request.
    pub fn lookup(&mut self, pending_ref: TxId) -> Option<HoldData> {
        let hold = self.read(pending_ref)?;
        Some(self.with_group(hold))
    }

    /// Starts a lookup without waiting for the store. What the index gives is candidates — addresses
    /// whose fingerprint matches — and only a record settles which is really this key, so the walk stops
    /// at the first candidate that is in memory and matches, and otherwise asks the store for one and
    /// carries the rest of the walk with it. Answering "not there" on a fingerprint collision instead
    /// would reject a hold that exists, a few times in every ten thousand.
    pub fn begin_lookup(&mut self, handle: u64, key: TxId, now: u64) -> Started {
        let candidates = self.index.candidates(key);
        self.walk(
            Fetch {
                key,
                candidates,
                next: 0,
            },
            handle,
            now,
        )
    }

    /// The next lookup the store has finished, if its record turns out to be the key that was asked
    /// for. When it does not, the walk continues and this answers nothing yet.
    pub fn harvest(&mut self, now: u64) -> Option<(u64, Option<HoldData>)> {
        while let Some((handle, _, found, hold)) = self.records.harvest(now) {
            let Some(fetch) = self.fetches.remove(&handle) else {
                continue;
            };
            if found == fetch.key {
                return Some((handle, Some(self.with_group(hold))));
            }
            // A different hold shares the fingerprint. Keep walking; the answer is not in yet.
            match self.walk(fetch, handle, now) {
                Started::Answered(answer) => return Some((handle, answer)),
                Started::Fetching | Started::Busy => continue,
            }
        }
        None
    }

    /// Fetches asked of the store and not yet answered.
    pub fn inflight(&self) -> usize {
        self.fetches.len()
    }

    fn walk(&mut self, mut fetch: Fetch, handle: u64, now: u64) -> Started {
        while fetch.next < fetch.candidates.len() {
            let addr = fetch.candidates.address(fetch.next);
            fetch.next += 1;
            match self.records.try_read(addr) {
                Some((found, hold)) if found == fetch.key => {
                    return Started::Answered(Some(self.with_group(hold)))
                }
                // In memory and somebody else's: the fingerprint matched and the key did not. With a
                // whole hash for a fingerprint this is beyond unreachable in practice — it cannot be
                // constructed by search, which is why there is no test for it — but "beyond unreachable"
                // is a probability and not a proof, and answering absent here would reject a hold that
                // exists. It was reachable, and tested, while the fingerprint was sixteen bits.
                Some(_) => continue,
                None => {
                    if !self.records.fetch(handle, addr, now) {
                        return Started::Busy;
                    }
                    self.fetches.insert(handle, fetch);
                    return Started::Fetching;
                }
            }
        }
        Started::Answered(None)
    }

    fn with_group(&self, mut hold: HoldData) -> HoldData {
        if let Some(state) = self.budgets.get(&hold.budget) {
            hold.budget_members = state.members;
            hold.budget_remaining = state.remaining;
        }
        hold
    }

    /// Whether the record at this address carries this key. Only asked when the index says a fingerprint
    /// is shared, which in a deployment that has never seen a collision is never.
    /// Whether the index already has this key. Reads a record where a fingerprint is shared, which is why
    /// only `replay` asks — see `Arrival`.
    fn holds(&mut self, key: TxId) -> bool {
        let Self { index, records, .. } = self;
        let mut verify = Self::verifier(records, key);
        index.addr_of(key, &mut verify).is_some()
    }

    fn verifier(records: &mut RecordLog, key: TxId) -> impl FnMut(RecordAddr) -> bool + '_ {
        move |addr| records.read(addr).is_some_and(|(found, _)| found == key)
    }

    /// Committed decisions applied so far. An answer carries this, so the sequencer can tell an answer
    /// taken after the writes it had sent from one taken before them.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// `Err` is the one thing a write can fail at: an index that cannot take a new hold. Only a
    /// `Create` inserts — a `Reduce` repoints an existing slot and a `Remove` frees one — so it is the
    /// only variant that can report it.
    pub fn write(&mut self, effect: PendingEffect, at: ApplyIndex) -> Result<(), NotStored> {
        self.apply_effect(effect, at, Arrival::First)
    }

    /// The same effect arriving again, which is what recovery does between a snapshot's coverage and now.
    ///
    /// Safe to call on state that already reflects the effect, which is the property the whole boundary rests
    /// on (design notes §15): a `Remove` of a hold that is gone returns before touching anything, a `Reduce`
    /// repoints to the same or a newer address and appends one wasted record version, a `Reduce`'s group
    /// arithmetic does not exist because a group member cannot be resolved in part, and a `Create` becomes a
    /// repoint here rather than a second slot for one key.
    /// `groups_reflect` is the position the restored group totals already reflect, which the snapshot's
    /// header carries. It is an argument rather than state the engine keeps because a caller cannot then
    /// forget to supply it: replay without it silently counts a member twice, and the totals are the one part
    /// of a snapshot that is accumulated rather than derived.
    pub fn replay(
        &mut self,
        effect: PendingEffect,
        at: ApplyIndex,
        groups_reflect: ApplyIndex,
    ) -> Result<(), NotStored> {
        self.apply_effect(effect, at, Arrival::Again(groups_reflect))
    }

    fn apply_effect(
        &mut self,
        effect: PendingEffect,
        at: ApplyIndex,
        arrival: Arrival,
    ) -> Result<(), NotStored> {
        self.applied += 1;
        // Recorded before the effect, so a block sealed while this one is being written is stamped with the
        // position *before* it rather than after. Coverage errs early, which costs replay a little and
        // cannot lose anything; erring late would claim a record was sealed that is not.
        self.applied_through = at;
        match effect {
            PendingEffect::Create {
                tx_id,
                debit_account,
                credit_account,
                amount,
                ledger,
                budget,
            } => {
                // Already here means this effect is arriving again, so the hold keeps its place in the
                // index and its place in the group: a second insert would give one key two slots, and one
                // `remove` would then clear only one of them — a resolved hold alive again, its money
                // reserved for good.
                let already = matches!(arrival, Arrival::Again(_)) && self.holds(tx_id);
                let stored = self.put(
                    tx_id,
                    HoldData {
                        debit_account,
                        credit_account,
                        amount,
                        remaining: amount,
                        ledger,
                        budget,
                        budget_members: 0,
                        budget_remaining: 0,
                    },
                    !already,
                );
                // Counted once, and `already` is not the question. A snapshot carries the group totals as of
                // its own instant and the slots as of its coverage, which is earlier — so a member can be in
                // the totals and out of the index at the same time, and asking the index would count it
                // twice. What decides is whether the totals already reflect this position.
                //
                // Measured on the first test that tried it the other way: a group of 303 came back as 456.
                let counted = match arrival {
                    Arrival::First => ApplyIndex::default(),
                    Arrival::Again(reflect) => reflect,
                };
                if !budget.is_absent() && at > counted {
                    let state = self.budgets.entry(budget).or_default();
                    state.members += 1;
                    state.remaining += amount;
                }
                if !stored {
                    return Err(NotStored { hold: tx_id });
                }
            }
            // Nothing is read when the overlay still had the record: append-only needs the whole record
            // to write it again, and the decision brought it. Without it — a resolution judged inside the
            // chain that created the hold, whose overlay entry the sequencer never had — the old version
            // is read back, which is what this used to cost every time.
            PendingEffect::Reduce {
                pending_ref,
                debit_account,
                credit_account,
                amount,
                remaining,
                ledger,
                budget,
            } => {
                // Everything the record needs came with the decision, unless the sequencer could not
                // supply the hold's original size — then the old version is read for it.
                let amount = if amount > 0 {
                    amount
                } else {
                    match self.read(pending_ref) {
                        Some(old) => old.amount,
                        None => return Ok(()),
                    }
                };
                let hold = HoldData {
                    debit_account,
                    credit_account,
                    amount,
                    remaining,
                    ledger,
                    budget,
                    budget_members: 0,
                    budget_remaining: 0,
                };
                // A repoint, not an insert: the slot is already there, so this cannot overflow.
                self.put(pending_ref, hold, false);
                // A group's total is deliberately not adjusted here, because a `Reduce` cannot belong to a
                // group: `BudgetRules::allow_resolution` refuses a partial resolution of any hold that has
                // one, whatever its size, so a group member always resolves in full and becomes a `Remove`.
                //
                // Asserted rather than assumed, and here rather than only where the rule lives, because
                // something else now depends on it. Every other write is safe to apply twice — a `Remove`
                // of a hold that is gone returns early, a repoint sets the same address again — so replay
                // over already-applied state is idempotent, which is what lets a checkpoint carry the index
                // as it is now and let the log's tail rebuild the rest. A subtraction that ran twice would
                // be the one exception, and it would be silent: it took a hand-built effect the judge
                // would have refused to produce a group remaining of minus twenty.
                debug_assert!(
                    budget.is_absent(),
                    "a partial resolution reached the store with a budget group"
                );
            }
            // Nothing is read: the slot is found by address and the group's arithmetic came with the
            // decision. This is every full settle and every void, which is most of the traffic.
            PendingEffect::Remove {
                pending_ref,
                budget,
                released,
            } => {
                let Self { index, records, .. } = self;
                let mut verify = Self::verifier(records, pending_ref);
                if index.remove(pending_ref, &mut verify).is_none() {
                    return Ok(());
                }
                if budget.is_absent() {
                    return Ok(());
                }
                let Some(state) = self.budgets.get_mut(&budget) else {
                    return Ok(());
                };
                // No guard here: the decrement is behind the index removal, which finds nothing the second
                // time and returns above before reaching a total.
                state.members = state.members.saturating_sub(1);
                state.remaining -= released;
                if state.members == 0 {
                    self.budgets.remove(&budget);
                }
            }
        }
        Ok(())
    }

    /// The maps are the engine's own, so their size is its to report; the gauges are where another
    /// thread may read it. The index's capacity is its slots rather than its entries: what a hold
    /// costs in the index is a slot whether or not one is occupied.
    pub(crate) fn publish(
        &self,
        holds: &MapGauge,
        budgets: &MapGauge,
        blocks: &MapGauge,
        buffer: &MapGauge,
        resident: &MapGauge,
    ) {
        holds.publish(self.index.live(), self.index.slots());
        budgets.publish(self.budgets.len(), self.budgets.capacity());
        let (buffered, in_memory, stored) = self.records.blocks();
        let (flush_window, resident_window) = self.records.windows();
        // A store block is written once and freed only when its segment expires, so the count is its
        // own ceiling; the two memory zones each have the window they were given.
        blocks.publish(stored, stored);
        buffer.publish(buffered, flush_window);
        resident.publish(in_memory, resident_window);
    }

    pub fn traffic(&self) -> LogTraffic {
        let (_, worst_cascade) = self.index.kick_stats();
        LogTraffic {
            index_live: self.index.live(),
            index_slots: self.index.slots(),
            worst_cascade,
            ambiguous: self.index.ambiguous(),
            overflowed: self.overflowed,
            segment: self.records.segment(),
            days_behind: self.days_behind(),
            days_of_slack: self.days_of_slack(),
            swept_blocks: self.swept_blocks,
            ..self.records.traffic()
        }
    }

    /// Carries the oldest buffered block's survivors on and drops the rest. A record is alive exactly
    /// when the index still points at it, which needs no extra bookkeeping: a resolved hold has no
    /// entry and a superseded version's entry has moved on. Both tests are address comparisons, so
    /// compaction reads nothing it has not already got in hand.
    fn compact(&mut self) {
        while self.records.over_window() {
            self.survivors.clear();
            let mut died = 0;
            for (key, hold, addr) in self.records.oldest_block() {
                if self.index.points_at(key, addr) {
                    self.survivors.push((key, hold, addr));
                } else {
                    died += 1;
                }
            }
            // The block being drained carries its position on to the one being filled, so coverage knows
            // where the records that are out of the buffer but not on the store begin.
            let from = self.records.oldest_began_at();
            for index in 0..self.survivors.len() {
                let (key, hold, old) = self.survivors[index];
                let new = self.records.keep(key, &hold, from);
                self.index.replace(key, old, new);
            }
            self.records.drop_oldest(died);
        }
    }

    /// Appends the record and points the key at it. Both halves are one call because a record the
    /// index does not point at is unreachable and an index entry with no record is a wrong answer.
    ///
    /// `fresh` is what lets the index check uniqueness: a hold the store has never held is the one moment
    /// a shared fingerprint can be noticed for free.
    ///
    /// False when the index could not take it. The counter and the caller's answer are set by this one
    /// call, so the number a report prints and the news the sequencer acts on cannot disagree.
    fn put(&mut self, key: TxId, hold: HoldData, fresh: bool) -> bool {
        let addr = self.records.append(key, &hold, self.applied_through);
        let Self { index, records, .. } = self;
        if fresh {
            if index.insert_new(key, addr).is_err() {
                // The table was sized for a maximum this has passed. Nothing here can fix it, and the
                // hold is not written down — so it is both counted, for the report, and handed back, so
                // the sequencer can stop applying decisions it cannot store.
                self.overflowed += 1;
                return false;
            }
        } else {
            let mut verify = Self::verifier(records, key);
            index.repoint(key, addr, &mut verify);
        }
        self.compact();
        true
    }

    /// Moves the engine to a new day, and marks the day that has now run out for the sweep below.
    ///
    /// One clock reading per day, taken by whoever runs the loop and handed in — the engine keeps no clock
    /// of its own, exactly as it keeps none for a lookup's `now`. A day rather than a finer unit because a
    /// segment is what space is reclaimed in, and `grace_days` is what makes a whole-day granularity safe:
    /// deletion lands between `retention` and `retention + grace`, so it is never early.
    ///
    /// A segment is the day a record was **written**, not the day its hold was created: a record gets its
    /// address when the writeback buffer compacts it out, up to a flush window later. That only ever
    /// delays expiry — a hold created at 23:59 and flushed after midnight is deleted with the next day's
    /// segment — and delay is the safe direction. The flush window is an hour against a grace of a day, so
    /// this rounding is inside the slack that is already paid for.
    ///
    /// Returns false when the day has not changed, so a caller may ask every round.
    pub fn open_day(&mut self, day: u64, lifetime_days: u64) -> bool {
        // Both of these are set even when the day has not moved, because a caller may ask every round and
        // the first ask is where the engine learns them. Leaving the lifetime at zero until the day changed
        // was a real defect: the sweep read "day zero has already run out", walked forward past the day its
        // own records were about to land in, and then never came back to it.
        self.lifetime_days = lifetime_days;
        if self.sweep.day.is_none() {
            self.sweep.day = Some(self.oldest_unfinished_day(day));
        }
        if day == self.today {
            return false;
        }
        if !self.day_has_a_segment(day) {
            return false;
        }
        self.today = day;
        self.records.open_day(day);
        true
    }

    /// Where the cursor starts: the oldest expired day the index still has entries in, or the oldest day
    /// that has expired if it has none.
    ///
    /// **This is what makes a leadership change safe, and it is the only reason the counts are read here.**
    /// The cursor is leader-local and volatile — deliberately, because which day has run out is a judgment
    /// from the leader's own clock and never a log entry (design notes §14). So a new leader starts without
    /// one. Deriving it from its clock alone, as `today - lifetime`, abandons every day the old leader was
    /// still working on: those holds are never released, their pending columns never come back down, and
    /// their blocks never go back. It is the same defect `Sweep` records having been found once already —
    /// a day abandoned because the walk restarted at whatever had just expired — reached the other way.
    ///
    /// The counts are the recovery, and they can be because they are a function of the log: a node that has
    /// applied the same prefix has the same counts, whatever its table size or its clock. A segment with
    /// entries in it whose day has already expired is a day somebody left unfinished, and the oldest of
    /// those is where to resume.
    ///
    /// Sound only while every live day has a segment of its own, which is what `day_has_a_segment` keeps
    /// true: past that a segment number no longer names one day and this could resume on the wrong one.
    fn oldest_unfinished_day(&self, today: u64) -> u64 {
        let Some(expired_through) = today.checked_sub(self.lifetime_days) else {
            return today.saturating_sub(self.lifetime_days);
        };
        (0..SEGMENTS)
            .filter(|&segment| self.index.live_in_segment(segment as u8) > 0)
            // The most recent day at or before the last expired one with this segment number. Unique
            // because a day's number is its day modulo `SEGMENTS` and no more than that many are live.
            .map(|segment| expired_through - (expired_through + SEGMENTS - segment) % SEGMENTS)
            .min()
            .unwrap_or(expired_through)
    }

    /// Whether opening this day would still leave every live day a segment number of its own.
    ///
    /// A segment's number is its day modulo `SEGMENTS`, so a day being written and a day the sweep has not
    /// emptied can come to share one — and then one block range covers two days and one count counts both.
    /// Neither is a wrong answer, because the walk enumerates block numbers that belong to the other day and
    /// they fail the index's address check: the day simply never finishes, for ever, and no later day does
    /// either. That is the safe direction and an unbounded amount of it.
    ///
    /// Refusing to open the day stops it. New records keep going into the segment already open, which dates
    /// them later than they are — late deletion, which is the direction that costs only space. And it
    /// releases itself: the moment the sweep finishes a day the span shrinks and the calendar moves again.
    ///
    /// **Rule 20.** `validate` already refuses a lifetime needing more segments than exist, but nothing
    /// refused a *sweep* far enough behind to need them. The ceiling was the address format's, enforced by
    /// whichever structure happened to misbehave first — so it was a limit nobody declared. It is declared
    /// here, from the one number it follows from.
    fn day_has_a_segment(&self, day: u64) -> bool {
        let Some(oldest) = self.sweep.day else {
            return true;
        };
        day.saturating_sub(oldest) < SEGMENTS
    }

    /// Days the sweep may still fall behind before the calendar stops. Zero means it has stopped, which is
    /// a state a run has to be able to see rather than infer from a formula.
    pub fn days_of_slack(&self) -> u64 {
        let Some(oldest) = self.sweep.day else {
            return SEGMENTS;
        };
        SEGMENTS.saturating_sub(self.today.saturating_sub(oldest) + 1)
    }

    /// The next holds whose retention has run out, as the resolutions that release them. Bounded per call
    /// by blocks of the expiring day, so a day's expiry is spread over the day rather than arriving as one
    /// burst — falling behind deletes late, which is safe, so this is a capacity dial and not a correctness
    /// one.
    ///
    /// **Blocks of the day, not slots of the index.** Both find the same survivors and both are exact: a
    /// record is alive exactly when the index points at it, so the walk needs no other bookkeeping either
    /// way. What differs is what a round costs. Searching the index for addresses in the segment bounded
    /// the voids *collected* and not the slots *visited*, and a day thinning towards empty runs out of
    /// voids long before it runs out of table — so the last rounds of every day walked all of it, 2.2
    /// seconds at the design's size, on this thread, ahead of the lookups waiting behind it. A day's own
    /// blocks are what that day wrote: bounded by declaration, and read sequentially.
    ///
    /// **The day is done when the index has nothing left in it**, which `live_in_segment` answers in
    /// constant time. That is what the whole-index pass was being used to find out, and it is the reason
    /// the count exists.
    pub fn propose_expiry(&mut self, blocks: usize, into: &mut Vec<Transfer>) {
        let Some(segment) = self.expiring_segment() else {
            return;
        };
        if self.index.live_in_segment(segment) == 0 {
            // The day's holds are all released. Its blocks are `reclaim`'s to hand back — that is not a
            // decision about retention and does not belong to the cursor.
            self.sweep = Sweep {
                day: self.sweep.day.map(|day| day + 1),
                at_block: 0,
            };
            return;
        }
        // Voids already handed over and not yet landed. Offered again rather than walked past, and the test
        // for "landed" is the same one the walk makes — a record is alive exactly when the index points at
        // it — so nothing has to be tracked on the apply path to know which of them are done.
        //
        // This is what the day's detail level of a timing wheel is for, at the size the throttle makes it:
        // one slice, not one day. Without it the only way to retry a lost void was to walk the day again,
        // which meant choosing between re-offering everything and re-offering nothing — and the gate that
        // chose "nothing" turned a void the sequencer declined into a day that never finished.
        let index = &self.index;
        self.outstanding
            .retain(|(addr, void)| index.points_at(void.pending_ref, *addr));
        if !self.outstanding.is_empty() {
            into.extend(self.outstanding.iter().map(|(_, void)| *void));
            return;
        }
        let index = &self.index;
        let outstanding = &mut self.outstanding;
        let mut visit = |key: TxId, hold: HoldData, addr: RecordAddr| {
            if !index.points_at(key, addr) {
                return;
            }
            // The one place an expiry void is built. Its id is derived from the hold, which is what makes
            // it a `TransferKind::VoidExpiry` everywhere downstream — the void flags alone would make it a
            // client's.
            let void = Transfer {
                id: TxId::expiry_void_of(key),
                pending_ref: key,
                debit_account: hold.debit_account,
                credit_account: hold.credit_account,
                // A void releases whatever is left, so it names no amount — which is also what makes it
                // safe to offer twice: the second is judged against a hold that is already gone.
                amount: 0,
                ledger: hold.ledger,
                flags: TransferFlags::VOID_PENDING,
            };
            outstanding.push((addr, void));
            into.push(void);
        };
        for _ in 0..blocks.max(1) {
            if !self
                .records
                .each_record_in_day(segment, self.sweep.at_block, &mut visit)
            {
                // The end of the day's blocks with holds still in it. Those holds are either behind a void
                // in flight — which `outstanding` is now holding, so the next round offers it again rather
                // than walking — or they were never reached because the walk stopped short. Either way the
                // walk starts over, and the day stays open, which is late rather than wrong.
                self.sweep.at_block = 0;
                return;
            }
            self.sweep.at_block += 1;
            self.swept_blocks += 1;
        }
    }

    /// Blocks of any day nothing points into any more, handed back to the store. Answers how many.
    ///
    /// **No clock, no cursor, no leadership, and no notion of retention.** A segment the index has no entry
    /// in holds only dead records — that is what the count means — and it is equally true of a day whose
    /// retention ran out and of one whose holds all resolved the ordinary way weeks early. So this is pure
    /// local housekeeping, and it has to be: on a follower nothing proposes voids, so a reclaim tied to the
    /// expiry cursor would leave its store growing while the leader's shrank. Every node runs it for itself
    /// and they need not agree on when.
    ///
    /// It also reclaims sooner than expiry would. Waiting for the retention window was never the rule —
    /// it was an artefact of the search: finding a day empty used to cost a pass over the index, so only
    /// the one day that had to be checked was. Sixty-three counts cost nothing to read.
    ///
    /// The segment being written is skipped. Its open block has been promised addresses and is not sealed,
    /// so its count says nothing about the blocks still to come; the space returns when the day rolls.
    pub fn reclaim(&mut self) -> usize {
        let open = self.records.segment();
        let mut freed = 0;
        for segment in 0..SEGMENTS as u8 {
            if segment == open || self.index.live_in_segment(segment) > 0 {
                continue;
            }
            // Asked so a day nothing was written to is not counted as freed. Freeing itself is one call now
            // — a segment stops existing — so this is about the answer rather than about the cost.
            if self.records.blocks_in_day(segment) > 0 {
                freed += self.records.free_segment(segment);
            }
        }
        freed
    }

    /// The oldest day still waiting to be emptied, and how many such days there are. Days are finished
    /// in order, so the first is the oldest unfinished one rather than whatever expired most recently.
    /// `None` when nothing has run out.
    ///
    /// One rule, one place: the segment to empty, whether a sweep is owed at all, and how far behind it
    /// is are three readings of this one fact.
    fn behind(&self) -> Option<(u64, u64)> {
        let day = self.sweep.day?;
        let expired_through = self.today.checked_sub(self.lifetime_days)?;
        Some((day, expired_through.checked_sub(day)? + 1))
    }

    /// The segment being emptied, if a day has run out.
    fn expiring_segment(&self) -> Option<u8> {
        self.behind().map(|(day, _)| (day % SEGMENTS) as u8)
    }

    /// The log position a snapshot of this engine would cover: everything up to it has reached a block, so
    /// replay starts after it and rebuilds what the writeback buffer still holds.
    ///
    /// The position the group totals reflect, which is everything this engine has applied. A snapshot carries
    /// it beside its coverage because the two are different instants, and replay needs both.
    pub fn applied_through(&self) -> ApplyIndex {
        self.applied_through
    }

    /// Makes durable what has been sealed, and answers whether there was anything to make durable. Whoever
    /// runs the loop decides how often, the same way it decides when the day has moved: the engine keeps no
    /// policy of its own, and the cost of asking rarely is a coverage that lags rather than anything lost.
    pub fn sync(&mut self) -> bool {
        self.records.sync()
    }

    /// Whether the store has refused something since this was last asked. Taken as it is read, so the news
    /// is handed over once.
    pub fn take_store_fault(&mut self) -> bool {
        self.records.take_fault()
    }

    /// Device time the store's synchronous calls have cost since this was last asked. Whoever runs the loop
    /// owes it: it is time the thread would have spent inside a `pwrite`, an `fsync` or a `pread`, and no
    /// lookup gets served during it.
    pub fn take_store_charge(&mut self) -> u64 {
        self.records.take_store_charge()
    }

    /// Zero means it covers nothing, which is what an engine that has applied nothing reflects — and a
    /// legitimate snapshot rather than a missing one, since a follower starting from empty receives exactly
    /// that.
    pub fn coverage(&self) -> ApplyIndex {
        self.records.durable_through(self.applied_through)
    }

    /// A writer over this engine's state. Borrows it, so nothing is copied to be written — and so a caller
    /// cannot apply anything while a snapshot is in flight, which is the stable read the design asks for and
    /// the only form of it that exists yet (design notes §15).
    /// Starts one, and shadows the index so the read is stable while the engine keeps writing. Answers what
    /// the stream will be, so a caller can size or pace it.
    ///
    /// Only one at a time: a second would need a second shadow, and the reason for the first — a kick cascade
    /// moving an entry between buckets — makes two overlapping reads two different tables.
    pub fn begin_snapshot(&mut self) -> SnapshotWriter {
        self.index.begin_snapshot();
        SnapshotWriter::new(
            self.index.bucket_count(),
            &self.budgets,
            self.coverage(),
            self.applied_through,
        )
    }

    /// The next chunk of a snapshot, into a buffer the caller sizes. Zero when the stream is finished, and
    /// that is when the shadow goes: whatever is left in it was never read, which only happens if a caller
    /// abandoned the snapshot.
    pub fn next_snapshot_chunk(&mut self, writer: &mut SnapshotWriter, into: &mut [u8]) -> usize {
        let written = writer.next_chunk(into, &mut self.index, &self.records);
        if writer.is_complete() {
            self.index.end_snapshot();
        }
        written
    }

    /// Abandons one, dropping the buckets held aside for it.
    pub fn abandon_snapshot(&mut self) {
        self.index.end_snapshot();
    }

    /// Buckets held aside for a snapshot in progress: what the stable read is costing right now.
    pub fn shadowed_buckets(&self) -> usize {
        self.index.shadowed()
    }

    /// Puts a snapshot's group totals back, once its stream is complete. The index restores itself as the
    /// chunks arrive, because holding a second copy of it is what a chunked format exists to avoid.
    ///
    /// Nothing checks that the two halves came from the same stream: a reader that had taken a partial one
    /// answers `is_complete` with false, and it is the caller's business not to ask for the groups then.
    ///
    /// The position comes back with them, because the engine's state now reflects it — without that a
    /// snapshot taken before the first replayed effect would claim to cover nothing, and a buffered block
    /// would be stamped as if the log had never happened.
    pub fn restore(&mut self, groups: FxHashMap<BudgetGroup, BudgetState>, coverage: ApplyIndex) {
        self.budgets = groups;
        self.applied_through = coverage;
    }

    /// The table a snapshot's chunks are written into. Exposed for restore alone — the index is otherwise
    /// the engine's own, and every other caller reaches it through a method that means something.
    pub fn index_mut(&mut self) -> &mut HoldTable {
        &mut self.index
    }

    /// That the index's per-segment counts still add up to its entries. Constant time in the number of
    /// segments, so it belongs with the per-round self-invariants rather than with `audit`.
    ///
    /// What it catches is a slot written somewhere other than `set_slot`. That does not lose money — the
    /// slots themselves are right, only the summary is wrong — but it does mean a day that is empty is never
    /// declared empty, so its blocks are never handed back and the store stops shrinking. Silent, and
    /// visible only as capacity, which is exactly the kind of thing that goes unnoticed for a month.
    pub fn counts_agree(&self) -> bool {
        self.index.counts_agree()
    }

    /// How many expired days are still waiting to be emptied. One is the ordinary state — the day that
    /// just ran out is being worked through. More than `grace_days` is the throttle behind by longer than
    /// the slack the index was sized with: `declared_maximum` assumes a hold leaves within
    /// `retention + grace`, and a hold that stays past it is an insert the index cannot take, which seals.
    /// So this is the number that says whether "deleting late is safe" is still true of a run.
    pub fn days_behind(&self) -> u64 {
        self.behind().map_or(0, |(_, behind)| behind)
    }

    /// Whether a day is still waiting to be emptied at all.
    /// Blocks of expiring days the sweep has read. Exported for this crate's own bench: what a day of
    /// expiry costs can only be measured by driving the engine, and the walk it does is the whole cost.
    pub fn swept_blocks(&self) -> u64 {
        self.swept_blocks
    }

    pub fn sweeping(&self) -> bool {
        self.days_behind() > 0
    }

    /// Blocks handed back to the store, which is the only way it shrinks. Read from the log rather than
    /// counted again here: it is the one that frees them.
    pub fn freed_blocks(&self) -> u64 {
        self.records.traffic().freed
    }

    fn read(&mut self, key: TxId) -> Option<HoldData> {
        let addr = {
            let Self { index, records, .. } = self;
            let mut verify = Self::verifier(records, key);
            index.addr_of(key, &mut verify)?
        };
        self.records.read(addr).map(|(_, hold)| hold)
    }
}

/// Every test in this file is about what the engine does with a decision it *took*, so each one
/// asserts the insert landed rather than ignoring the answer: a setup whose index silently overflowed
/// would be a test about nothing. One place, because all four test modules need it.
#[cfg(test)]
mod test_support {
    use super::{ApplyIndex, PendingEffect, PendingEngine};

    pub trait Stored {
        /// Applies an effect at the next position, which is all a test that does not care about positions
        /// needs: one more than the last, so the sequence is monotonic the way a log is.
        fn stored(&mut self, effect: PendingEffect);
        /// Applies it at a position the test chose, for the tests that are about positions.
        fn stored_at(&mut self, effect: PendingEffect, at: ApplyIndex);
    }

    impl Stored for PendingEngine {
        fn stored(&mut self, effect: PendingEffect) {
            let at = ApplyIndex(self.applied() + 1);
            self.stored_at(effect, at);
        }

        fn stored_at(&mut self, effect: PendingEffect, at: ApplyIndex) {
            self.write(effect, at).expect("the index took the hold");
        }
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount, budget: BudgetGroup) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget,
        }
    }

    use crate::engine::test_support::Stored;

    /// The decision blocks are written once buys: a partial resolution appends a new version and the
    /// index follows it, instead of a block being read, changed and written back. What the engine
    /// answers has to be the new remainder, and the old record has to still be sitting there — that
    /// is the cost, and a version silently overwritten in place would hide it.
    #[test]
    fn a_partial_resolution_appends_a_version_rather_than_rewriting_one() {
        let mut engine = PendingEngine::default();
        engine.stored(create(TxId(9), 100, BudgetGroup::ABSENT));
        let (_, after_create) = (engine.records.blocks(), engine.records.appended());

        engine.stored(PendingEffect::Reduce {
            pending_ref: TxId(9),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 100,
            remaining: 40,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        assert_eq!(
            engine.records.appended(),
            after_create + 1,
            "the reduction did not append a record"
        );
        assert_eq!(engine.lookup(TxId(9)).map(|hold| hold.remaining), Some(40));

        engine.stored(PendingEffect::Reduce {
            pending_ref: TxId(9),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 100,
            remaining: 10,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        assert_eq!(engine.records.appended(), after_create + 2);
        assert_eq!(engine.lookup(TxId(9)).map(|hold| hold.remaining), Some(10));

        engine.stored(PendingEffect::Remove {
            pending_ref: TxId(9),
            budget: BudgetGroup::ABSENT,
            released: 10,
        });
        assert!(
            engine.lookup(TxId(9)).is_none(),
            "a removed hold is gone from the index"
        );
        assert_eq!(
            engine.records.appended(),
            after_create + 2,
            "a removal writes nothing; the records wait for their segment to expire"
        );
    }

    /// A group's totals follow the writes the engine is told about, across the block boundary its members
    /// spill over. Holds are found by index, so nothing depends on a record staying in the block it was
    /// first written to — and a group of more members than fit on one block is the case that says so.
    ///
    /// Resolved in full, because a group member cannot be resolved any other way:
    /// `BudgetRules::allow_resolution` refuses a partial resolution of a hold that has a group, whatever
    /// its size. This test used to use one anyway and assert what the engine did with it, which was
    /// asserting behaviour for a shape the rules make unreachable — the `debug_assert` in the `Reduce`
    /// arm found it.
    #[test]
    fn a_group_survives_its_records_spilling_into_later_blocks() {
        let group = BudgetGroup(77);
        let mut engine = PendingEngine::default();
        let members = RECORDS_PER_BLOCK + 3;
        for member in 0..members {
            engine.stored(create(TxId(member as u128 + 1), 10, group));
        }
        let hold = engine.lookup(TxId(1)).expect("the first member");
        assert_eq!(hold.budget_members, members as u32);
        assert_eq!(hold.budget_remaining, members as Amount * 10);

        engine.stored(PendingEffect::Remove {
            pending_ref: TxId(1),
            budget: group,
            released: 10,
        });
        let last = engine
            .lookup(TxId(members as u128))
            .expect("the last member");
        assert_eq!(last.budget_remaining, members as Amount * 10 - 10);
        assert_eq!(last.budget_members, members as u32 - 1);
    }

    /// The index does not grow, so an insert past its declared maximum has nowhere to go. It is named
    /// back to the caller rather than absorbed, because a hold the log says exists and the store does not
    /// have is not a number to report — it is news the sequencer has to stop on.
    #[test]
    fn a_hold_the_index_cannot_take_is_named_back_to_the_caller() {
        let mut engine = PendingEngine::sized(8, 64, 64, Box::new(MemoryStore::default()));
        let mut refused = None;
        for index in 0..256u128 {
            if let Err(not_stored) = engine.write(
                create(TxId(index + 1), 10, BudgetGroup::ABSENT),
                ApplyIndex(index as u64 + 1),
            ) {
                refused = Some(not_stored.hold);
                break;
            }
        }
        let refused = refused.expect("an index of eight slots cannot take 256 holds");
        // The hold it names is the one that was lost, which is what a diagnostic has to point at.
        assert!(engine.lookup(refused).is_none());
        assert_eq!(engine.traffic().overflowed, 1);
    }
}

#[cfg(test)]
mod apply_tests {
    use crate::engine::test_support::Stored;
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    /// The property a sixteen-byte slot buys: applying a committed decision reads **nothing**, however
    /// cold the hold is. A whole hash for a fingerprint makes a match identity, so no slot has to be
    /// confirmed against a record — and the decision carries the record a partial resolution needs to
    /// append. Apply is in order, so a read here would be an IO nothing can hide.
    #[test]
    fn applying_a_decision_never_reads_the_store() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let cold = TxId(1);
        engine.stored(create(cold, 500));
        let hold_amount = engine.lookup(cold).expect("created").amount;

        // Push it out of the buffer and into the store, then forget what that cost.
        for index in 0..RECORDS_PER_BLOCK * 3 {
            engine.stored(create(TxId(1_000 + index as u128), 10));
        }
        let before = engine.traffic().apply_store_reads;

        engine.stored(PendingEffect::Reduce {
            pending_ref: cold,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: hold_amount,
            remaining: 300,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        engine.stored(PendingEffect::Remove {
            pending_ref: cold,
            budget: BudgetGroup::ABSENT,
            released: 300,
        });
        assert_eq!(
            engine.traffic().apply_store_reads,
            before,
            "applying a decision about a cold hold read the store"
        );
        assert!(engine.lookup(cold).is_none(), "the hold was not resolved");
    }

    /// And when the decision carries no record — a resolution judged inside the chain that created the
    /// hold, which the sequencer's overlay never held — the engine reads it back. An optimisation with a
    /// fallback, not a fact anything rests on.
    #[test]
    fn a_decision_without_the_record_falls_back_to_reading_it() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let cold = TxId(2);
        engine.stored(create(cold, 500));
        for index in 0..RECORDS_PER_BLOCK * 3 {
            engine.stored(create(TxId(2_000 + index as u128), 10));
        }
        let before = engine.traffic().apply_store_reads;

        // No original size supplied, which is what sends the engine to the record it already has.
        engine.stored(PendingEffect::Reduce {
            pending_ref: cold,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 0,
            remaining: 100,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });
        assert!(
            engine.traffic().apply_store_reads > before,
            "the fallback did not read the record it needed"
        );
        assert_eq!(engine.lookup(cold).map(|hold| hold.remaining), Some(100));
    }

    /// The two windows are independent, and this is the property that says so: a record written to the
    /// store is still answered from memory, and it stops being answered from memory only when residency
    /// ends — not when it was written. Collapsing the two would make every flushed record an IO, which is
    /// the whole cost this exists to avoid.
    #[test]
    fn a_written_record_is_still_answered_from_memory_until_residency_ends() {
        // One block unwritten, two blocks written and kept. A survivor of the flush window therefore
        // lands in the store and stays readable, until two blocks' worth of later survivors push it out.
        let mut engine = PendingEngine::with_windows(1, 2);
        let survivor = TxId(7);
        engine.stored(create(survivor, 500));
        // Enough later holds to compact the survivor's block out of the buffer, but not enough to fill
        // residency: three blocks of survivors would, so two is the most that can be asked for here.
        for index in 0..RECORDS_PER_BLOCK * 2 {
            engine.stored(create(TxId(1_000 + index as u128), 10));
        }

        let carried = engine.traffic().flushed;
        assert!(
            carried > 0,
            "nothing was carried on, so the flush window never closed"
        );
        let before = engine.traffic();
        assert_eq!(engine.lookup(survivor).map(|hold| hold.amount), Some(500));
        let after = engine.traffic();
        assert_eq!(
            after.store_reads, before.store_reads,
            "a resident record cost an IO"
        );
        assert!(
            after.resident_reads > before.resident_reads,
            "it was not read from residency"
        );

        // Now push it past residency. Its content is on the store, so nothing is lost — but answering
        // for it is an IO from here on, which is exactly what `left_memory` counts.
        for index in 0..RECORDS_PER_BLOCK * 6 {
            engine.stored(create(TxId(9_000 + index as u128), 10));
        }
        assert!(
            engine.traffic().left_memory > 0,
            "residency never dropped a block"
        );
        let before = engine.traffic();
        assert_eq!(engine.lookup(survivor).map(|hold| hold.amount), Some(500));
        assert!(
            engine.traffic().store_reads > before.store_reads,
            "a record past residency was still answered from memory"
        );
    }
}

#[cfg(test)]
mod buffer_tests {
    use crate::engine::test_support::Stored;
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    fn resolve(tx_id: TxId, released: Amount) -> PendingEffect {
        PendingEffect::Remove {
            pending_ref: tx_id,
            budget: BudgetGroup::ABSENT,
            released,
        }
    }

    /// The saving the whole capacity estimate rests on: a hold resolved before its block is compacted
    /// never reaches the store. Without this the store would grow with holds created rather than holds
    /// alive, and the design's own figure for how much disk it needs would be the wrong one.
    #[test]
    fn a_hold_resolved_before_its_block_is_compacted_never_reaches_the_store() {
        // One block of window, so the second block being filled compacts the first.
        let mut engine = PendingEngine::with_windows(1, 1);
        let holds = RECORDS_PER_BLOCK;
        for index in 0..holds {
            engine.stored(create(TxId(index as u128 + 1), 10));
        }
        // Every one of them resolved while still buffered.
        for index in 0..holds {
            let key = TxId(index as u128 + 1);
            engine.stored(resolve(key, 10));
        }
        // One more record starts a second block, which is what puts the first one over the window.
        // Only that far: filling the second block too would compact live records and the count below
        // would be measuring those instead.
        engine.stored(create(TxId(1_000_000), 10));
        let traffic = engine.traffic();
        assert!(
            traffic.died_in_buffer >= holds as u64,
            "records were carried on that were dead"
        );
        assert_eq!(traffic.flushed, 0, "a dead record was written to the store");
        assert_eq!(
            traffic.store_reads, 0,
            "nothing should have been read from the store yet"
        );
    }

    /// And a hold that outlives the window is carried on with its index entry following it, so it is
    /// still found afterwards. Compaction moves addresses; an entry left behind would be a hold the
    /// log says exists and the engine cannot find.
    #[test]
    fn a_survivor_is_carried_on_and_still_found_at_its_new_address() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let survivor = TxId(7);
        engine.stored(create(survivor, 500));

        // Fill two blocks past it, so its block is compacted out.
        for index in 0..RECORDS_PER_BLOCK * 2 + 2 {
            engine.stored(create(TxId(1_000 + index as u128), 10));
        }
        let after = engine
            .lookup(survivor)
            .expect("the survivor is still there");
        assert_eq!(after.remaining, 500, "the survivor came back changed");
        assert!(
            engine.traffic().flushed > 0,
            "nothing was carried on at all"
        );

        // And it is still the hold the index says it is, at whatever address compaction gave it.
        engine.stored(resolve(survivor, 500));
        assert!(
            engine.lookup(survivor).is_none(),
            "the survivor was not resolved"
        );
    }

    /// A partial resolution's old version is dead the moment the index moves on, so compaction drops it
    /// without being told. That is what makes append-only affordable: the garbage is identified by the
    /// index rather than tracked.
    #[test]
    fn a_superseded_version_is_dropped_without_being_tracked() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let hold = TxId(11);
        engine.stored(create(hold, 100));
        let hold_amount = engine.lookup(hold).expect("created").amount;
        engine.stored(PendingEffect::Reduce {
            pending_ref: hold,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: hold_amount,
            remaining: 60,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        });

        for index in 0..RECORDS_PER_BLOCK * 2 + 2 {
            engine.stored(create(TxId(2_000 + index as u128), 10));
        }
        assert_eq!(engine.lookup(hold).map(|found| found.remaining), Some(60));
        let traffic = engine.traffic();
        assert!(
            traffic.died_in_buffer > 0,
            "the superseded version was carried on as if it were alive"
        );
    }
}

#[cfg(test)]
mod fetch_tests {
    use crate::engine::test_support::Stored;
    use ledger_base::{AccountId, BudgetGroup, TxId};

    use super::*;
    use crate::block::RECORDS_PER_BLOCK;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    /// Pushes enough records through that everything written before is compacted into the store.
    fn cool(engine: &mut PendingEngine, from: u128) {
        for index in 0..RECORDS_PER_BLOCK * 3 {
            engine.stored(create(TxId(from + index as u128), 10));
        }
    }

    fn answered(started: Started) -> Option<HoldData> {
        match started {
            Started::Answered(found) => found,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// A hold still in the buffer is answered where the command was taken off the queue: no fetch, so
    /// nothing to wait for. This is the common case and it must not pay for the cold one.
    #[test]
    fn a_buffered_hold_is_answered_without_a_fetch() {
        let mut engine = PendingEngine::with_windows(8, 8);
        engine.stored(create(TxId(3), 100));
        let found = answered(engine.begin_lookup(1, TxId(3), 0));
        assert_eq!(found.map(|hold| hold.remaining), Some(100));
        assert_eq!(engine.inflight(), 0, "a buffered hold should need no fetch");
    }

    /// A hold the buffer no longer holds is fetched, and the answer arrives from the harvest rather
    /// than from the call. This is the path a device's latency lands on.
    #[test]
    fn a_cold_hold_is_fetched_and_answered_when_it_completes() {
        let mut engine = PendingEngine::with_windows(1, 1);
        let cold = TxId(1);
        engine.stored(create(cold, 250));
        cool(&mut engine, 1_000);

        match engine.begin_lookup(7, cold, 0) {
            Started::Fetching => {}
            other => panic!("a cold hold should be fetched, got {other:?}"),
        }
        assert_eq!(engine.inflight(), 1);

        let (handle, found) = engine.harvest(0).expect("the fetch completes");
        assert_eq!(handle, 7, "the answer came back under another handle");
        assert_eq!(found.map(|hold| hold.remaining), Some(250));
        assert_eq!(engine.inflight(), 0);
        assert!(engine.harvest(0).is_none(), "nothing else was outstanding");
    }

    /// A hold nobody ever created is answered as absent, and asking again gets the same answer without
    /// touching the store.
    #[test]
    fn a_hold_that_was_never_created_is_answered_absent() {
        let mut engine = PendingEngine::with_windows(1, 1);
        cool(&mut engine, 1);
        assert!(answered(engine.begin_lookup(1, TxId(999_999), 0)).is_none());
        assert_eq!(engine.inflight(), 0);
    }
}

/// Retention as the engine keeps it: a segment is a day, a day that runs out is emptied by releasing
/// whatever survived it, and the blocks go back once nothing points into them.
#[cfg(test)]
mod expiry_tests {
    use crate::block::RECORDS_PER_BLOCK;
    use crate::engine::test_support::Stored;
    use ledger_base::{AccountId, BudgetGroup, TransferKind, TxId};

    use super::*;

    /// One day of promised retention plus one of grace.
    const LIFETIME: u64 = 2;

    fn create(tx_id: TxId, amount: Amount) -> PendingEffect {
        PendingEffect::Create {
            tx_id,
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount,
            ledger: 1,
            budget: BudgetGroup::ABSENT,
        }
    }

    /// A record belongs to a day only once the writeback buffer has compacted it out, so a test about what
    /// day a record belongs to writes more than a block's worth. What is left in the open block has no
    /// segment yet, which is correct: an unwritten record is not one retention has reached.
    fn written_on_day_zero() -> (PendingEngine, usize) {
        let mut engine = PendingEngine::with_windows(1, 64);
        let holds = RECORDS_PER_BLOCK * 2 + 1;
        for id in 1..=holds {
            engine.stored(create(TxId(id as u128), 10));
        }
        (engine, holds - holds % RECORDS_PER_BLOCK)
    }

    fn offered(engine: &mut PendingEngine, blocks: usize) -> Vec<Transfer> {
        let mut voids = Vec::new();
        engine.propose_expiry(blocks, &mut voids);
        voids
    }

    /// What the sequencer does with an offered void once it commits: the hold is gone and its index entry
    /// with it. Nothing advances without this, which is the point — a day is finished when its holds are
    /// actually released, not when they have been offered.
    fn commit(engine: &mut PendingEngine, voids: &[Transfer]) {
        for void in voids {
            engine.stored(PendingEffect::Remove {
                pending_ref: void.pending_ref,
                budget: BudgetGroup::ABSENT,
                released: 10,
            });
        }
    }

    /// Offers and commits until the day is done, answering how many holds were released and in how many
    /// rounds. Bounded, so a sweep that never converged fails rather than hanging.
    ///
    /// Reclaims every round, the way the worker does. The two halves are separate jobs — proposing a void
    /// needs the leader's clock, handing back a dead day's blocks needs nothing — so a test that drove only
    /// the proposing half would be driving a node that never reclaims.
    fn empty_the_day(engine: &mut PendingEngine, blocks_per_round: usize) -> (usize, usize) {
        let mut released = 0;
        let mut rounds = 0;
        while engine.sweeping() {
            engine.reclaim();
            let voids = offered(engine, blocks_per_round);
            // Blocks bound the work and the voids both: a round reads the blocks it was asked for and every
            // void it offers came out of one of them. That is the property the bound being on blocks buys —
            // a bound on voids alone said nothing about how much was read to find them.
            assert!(
                voids.len() <= blocks_per_round * RECORDS_PER_BLOCK,
                "a round offered more voids than the blocks it read could hold"
            );
            commit(engine, &voids);
            released += voids.len();
            rounds += 1;
            assert!(rounds < 100_000, "the sweep never finished");
        }
        // Once more with the cursor past the day: the round that advanced it had already reclaimed, so the
        // blocks of the day it just left are handed back here.
        engine.reclaim();
        (released, rounds)
    }

    /// A day is a segment, and moving to a new day moves where records are written. Block numbers carry on
    /// across the boundary rather than restarting, which is what keeps an address unique on the block field
    /// alone — everything that finds a block by number then needs no notion of segments.
    #[test]
    fn a_new_day_is_a_new_segment_and_records_stay_findable_across_it() {
        let mut engine = PendingEngine::with_windows(1, 64);
        engine.stored(create(TxId(1), 100));
        let first = engine.records.segment();

        assert!(engine.open_day(1, LIFETIME));
        assert_ne!(engine.records.segment(), first, "the day did not move");
        assert!(!engine.open_day(1, LIFETIME), "the same day opened twice");

        engine.stored(create(TxId(2), 100));
        assert!(
            engine.lookup(TxId(1)).is_some(),
            "yesterday's hold was lost"
        );
        assert!(engine.lookup(TxId(2)).is_some(), "today's hold was lost");
    }

    /// The edge that matters. Deleting late costs space; deleting early refuses a resolution that was still
    /// entitled to arrive, which is a wrong answer — so nothing is offered until the promise and its grace
    /// have both passed.
    #[test]
    fn nothing_is_released_before_the_promise_and_its_grace_have_passed() {
        let (mut engine, _) = written_on_day_zero();
        for day in 1..LIFETIME {
            engine.open_day(day, LIFETIME);
            assert!(!engine.sweeping(), "day {day} thought a day had run out");
            assert!(
                offered(&mut engine, 1024).is_empty(),
                "day {day} released a hold whose lifetime had not run out"
            );
        }
    }

    /// Once it has, every survivor of that day is offered as a void naming both its accounts — which is why
    /// the sweep reads the record rather than working from the index alone. The id is derived from the hold,
    /// so two leaders propose the same one and the second is a duplicate rather than a second void.
    #[test]
    fn an_outlived_hold_is_offered_as_an_expiry_void_with_a_derived_id() {
        let (mut engine, written) = written_on_day_zero();
        engine.open_day(LIFETIME, LIFETIME);

        let voids = offered(&mut engine, 1024);
        assert_eq!(
            voids.len(),
            written,
            "not every survivor of the day was offered"
        );
        let void = voids[0];
        assert_eq!(void.id, TxId::expiry_void_of(void.pending_ref));
        // The kind, not the bit it is read from: what every stage downstream branches on is that this is
        // an expiry void and not a client's, and a build that lost the derivation would still say
        // `VoidClient` here while every assertion about the id passed.
        assert_eq!(void.kind(), Ok(TransferKind::VoidExpiry));
        assert_eq!(void.debit_account, AccountId(1));
        assert_eq!(void.credit_account, AccountId(2));
        // A void releases whatever is left, so it names no amount — which is also what makes offering the
        // same one twice harmless.
        assert_eq!(void.amount, 0);
        assert_eq!(void.flags, TransferFlags::VOID_PENDING);
    }

    /// A hold resolved before its retention ran out is not offered: the index no longer points at its
    /// record, which is the same test compaction uses. The dead are never read.
    #[test]
    fn a_resolved_hold_is_never_offered() {
        let (mut engine, written) = written_on_day_zero();
        engine.stored(PendingEffect::Remove {
            pending_ref: TxId(1),
            budget: BudgetGroup::ABSENT,
            released: 10,
        });
        engine.open_day(LIFETIME, LIFETIME);

        let voids = offered(&mut engine, 1024);
        assert_eq!(voids.len(), written - 1, "the resolved hold was offered");
        assert!(
            voids.iter().all(|void| void.pending_ref != TxId(1)),
            "the resolved hold was offered"
        );
    }

    /// A day's holds are offered a bounded number at a time and the sweep resumes where it stopped, so a
    /// day's expiry spreads over the day instead of arriving as one burst. Falling behind releases late,
    /// which is safe — so this is a capacity dial and not a correctness one.
    ///
    /// The bound is blocks of the day, and that is the half worth testing: a bound on voids collected left
    /// the work to find them unbounded, which is how a round came to walk the whole index.
    #[test]
    fn a_days_holds_are_released_a_bounded_number_at_a_time() {
        let (mut engine, written) = written_on_day_zero();
        engine.open_day(LIFETIME, LIFETIME);

        let blocks_per_round = 1;
        let (released, rounds) = empty_the_day(&mut engine, blocks_per_round);
        assert_eq!(released, written, "the sweep lost holds along the way");
        assert!(
            rounds > written / (blocks_per_round * RECORDS_PER_BLOCK),
            "the day came in too few rounds"
        );
    }

    /// The blocks go back only once nothing points into the day, which is the one moment they are known to
    /// be dead. This is the only way the store shrinks — records are written once and never rewritten — so
    /// without it a run's total is holds created rather than holds alive.
    #[test]
    fn a_days_blocks_go_back_once_nothing_points_into_it() {
        let (mut engine, written) = written_on_day_zero();
        assert_eq!(engine.freed_blocks(), 0, "blocks went back too early");

        // Counted after the day has moved, because moving it seals the open block: a block whose records
        // straddled two segments could be freed by neither day.
        engine.open_day(LIFETIME, LIFETIME);
        let blocks = engine.records.blocks().2;
        assert!(blocks > 0, "the test wrote no blocks to free");

        let (released, _) = empty_the_day(&mut engine, 1024);

        assert_eq!(released, written);
        assert_eq!(
            engine.freed_blocks(),
            blocks as u64,
            "the day emptied and its blocks did not go back"
        );
        assert_eq!(engine.records.blocks().2, 0, "the store did not shrink");
    }

    /// A freed day leaves no slot behind, which is why this index needs no epoch.
    ///
    /// The engine's design gives the index a `min_live_seg_id`, and it needs one: there, `apply_expire`
    /// unlinks a segment on a *time* condition, so slots addressing it survive the unlink and two things
    /// have to cope with them — a lookup answers Dead by comparing the segment against the epoch, and an
    /// insert treats such a slot as empty so a 90%-full table does not kick around the dead.
    ///
    /// Here the condition is different in kind: a day's blocks go back only once `live_in_segment` is zero,
    /// so the dead slots are gone *before* the blocks are, and both jobs have nothing to act on — a lookup
    /// finds no slot at all rather than a stale one, and an insert sees a genuinely empty slot. The epoch is
    /// absent because the ordering makes it unnecessary, not because it was skipped, and this test is what
    /// says so: change the free condition to a time and it fails.
    #[test]
    fn a_freed_day_leaves_no_slot_behind_so_the_index_needs_no_epoch() {
        let (mut engine, written) = written_on_day_zero();
        let day_zero = engine.records.segment();
        engine.open_day(LIFETIME, LIFETIME);

        let (released, _) = empty_the_day(&mut engine, 1024);
        assert_eq!(released, written);
        assert!(engine.freed_blocks() > 0, "no day was freed");

        assert_eq!(
            engine.index.live_in_segment(day_zero),
            0,
            "a slot still addresses a day whose blocks have gone back"
        );
        for id in 1..=written {
            assert!(
                engine.lookup(TxId(id as u128)).is_none(),
                "an expired hold was answered from a freed day"
            );
        }
    }

    /// The calendar stops rather than letting two live days share a segment.
    ///
    /// A segment's number is its day modulo `SEGMENTS`, so a sweep far enough behind meets its own target
    /// again as the day being written. Driving the engine there by hand showed what happens: the walk
    /// enumerates block numbers belonging to the other day, they fail the index's address check, no void is
    /// offered, and the day never finishes — for ever, and with it every later day. Refusing to open the day
    /// is what makes that state unreachable, and it releases itself as soon as the sweep finishes one.
    #[test]
    fn the_calendar_stops_before_two_live_days_share_a_segment() {
        let (mut engine, _) = written_on_day_zero();
        engine.open_day(LIFETIME, LIFETIME);

        // Nothing is ever committed, so day zero never finishes and the sweep never advances.
        let mut day = LIFETIME;
        for _ in 0..(SEGMENTS * 2) {
            day += 1;
            engine.open_day(day, LIFETIME);
            let mut voids = Vec::new();
            engine.propose_expiry(1, &mut voids);
        }

        assert_eq!(
            engine.days_of_slack(),
            0,
            "the calendar did not stop at the slack it has"
        );
        assert_ne!(
            engine.records.segment(),
            engine.expiring_segment().expect("a day has run out"),
            "the day being written shares a segment with the day being emptied"
        );

        // And it releases itself: finish the days it was stuck on and the calendar moves again. The *next*
        // day, not the one the loop reached — a jump of sixty-seven days at once is refused for the same
        // reason and by the same rule, which is a property worth having and not the one being tested here.
        let (_, _) = empty_the_day(&mut engine, 1024);
        assert!(engine.days_of_slack() > 0, "the calendar stayed stopped");
        assert!(
            engine.open_day(engine.today + 1, LIFETIME),
            "the day would not open once the sweep had caught up"
        );
    }

    /// A new leader resumes on the oldest day the index says is unfinished, not on whatever its clock makes
    /// the oldest expired one.
    ///
    /// The cursor is leader-local and volatile on purpose — which day has run out is a judgment from the
    /// leader's clock, never a log entry — so a new leader starts without one. Taking it from the clock
    /// abandons every day the old leader had not finished: their holds are never released and their pending
    /// columns never come down. The counts are the recovery because they are a function of the log.
    ///
    /// Two engines here rather than one, with the same holds written into the same day, standing in for two
    /// nodes that applied the same prefix. The second is told a much later day, as a new leader would be.
    #[test]
    fn a_new_leader_resumes_on_the_oldest_day_the_counts_say_is_unfinished() {
        let (mut old_leader, _) = written_on_day_zero();
        old_leader.open_day(LIFETIME, LIFETIME);
        assert_eq!(old_leader.days_behind(), 1, "day zero has not run out");

        // The same log, applied by a node that has not been leading — so no cursor, and a clock that has
        // moved well past the day the old leader is still stuck on.
        let (mut new_leader, _) = written_on_day_zero();
        let much_later = LIFETIME + 20;
        new_leader.open_day(much_later, LIFETIME);

        assert_eq!(
            new_leader.expiring_segment(),
            old_leader.expiring_segment(),
            "the new leader resumed on a different day and abandoned day zero"
        );
        assert_eq!(
            new_leader.days_behind(),
            much_later - LIFETIME + 1,
            "the new leader did not see how far behind the day it resumed on is"
        );

        // And it releases day zero's holds rather than leaving them.
        let (released, _) = empty_the_day(&mut new_leader, 1024);
        assert!(released > 0, "the new leader released nothing of day zero");
    }

    /// A void nobody took is offered again, which is what every "the sweep offers it again" comment in this
    /// code claimed and none of them delivered.
    ///
    /// The sequencer declines an expiry void when its backlog is full and refuses one when the lane is
    /// quarantined, and both are counted and neither is answered — so the sweep re-offering is the only
    /// retry there is. It used to re-offer by walking the day again, and it would not walk again until the
    /// day's live count moved. A declined void that was the last of its day therefore moved nothing, so the
    /// day never finished — and because days are emptied in order, no later day did either. Deletion stopped
    /// for the life of the node, silently, on one dropped notice.
    ///
    /// Here nothing is committed, which is exactly what a declined void looks like to the engine.
    #[test]
    fn a_void_nobody_took_is_offered_again() {
        let (mut engine, written) = written_on_day_zero();
        engine.open_day(LIFETIME, LIFETIME);

        let first = offered(&mut engine, 1);
        assert!(!first.is_empty(), "the day offered nothing to begin with");

        // Twenty rounds, nothing applied. The same voids have to keep coming back.
        for round in 0..20 {
            let again = offered(&mut engine, 1);
            let ids: Vec<TxId> = again.iter().map(|void| void.id).collect();
            let expected: Vec<TxId> = first.iter().map(|void| void.id).collect();
            assert_eq!(
                ids, expected,
                "round {round} offered something other than the slice nobody took"
            );
        }
        assert!(engine.sweeping(), "the day finished with holds still in it");

        // And once they are applied the walk moves on, so retrying is not a loop the day cannot leave.
        commit(&mut engine, &first);
        let (released, _) = empty_the_day(&mut engine, 1);
        assert_eq!(
            released + first.len(),
            written,
            "the day did not finish after its first slice was taken"
        );
    }

    /// A day is not finished while any of its holds is still there, and a hold nothing releases keeps its
    /// day open for ever rather than being skipped. An earlier version restarted the walk at whatever had
    /// just expired, so a day still being walked when the next arrived was abandoned — and its holds were
    /// never released at all.
    ///
    /// The days that ran out meanwhile pile up, and the count is what says so: a bool could not tell one
    /// day behind from four, and four is where the index has outlived the slack it was sized with.
    #[test]
    fn a_day_with_a_hold_left_is_never_finished_or_skipped() {
        let (mut engine, _) = written_on_day_zero();
        engine.open_day(LIFETIME, LIFETIME);

        // Offer and commit everything but one, then let several more days arrive.
        let voids = offered(&mut engine, 1024);
        let (keep, release) = voids.split_first().expect("voids to commit");
        commit(&mut engine, release);
        assert_eq!(engine.days_behind(), 1, "one day has run out");
        for day in LIFETIME + 1..LIFETIME + 4 {
            engine.open_day(day, LIFETIME);
            let again = offered(&mut engine, 1024);
            assert!(
                again
                    .iter()
                    .all(|void| void.pending_ref == keep.pending_ref),
                "a later day was worked on while an earlier one still had a hold"
            );
        }
        assert_eq!(
            engine.days_behind(),
            4,
            "the unfinished day was abandoned, or the days behind it did not pile up"
        );
        assert_eq!(
            engine.freed_blocks(),
            0,
            "blocks went back with a hold left"
        );

        commit(&mut engine, std::slice::from_ref(keep));
        empty_the_day(&mut engine, 1024);
        assert!(engine.freed_blocks() > 0, "the blocks never went back");
    }
}
