/// Counters for what happens per request; anything rarer is a log event instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    /// Reactor loop iterations. Compared with the others, this shows how much work a tick did.
    pub ticks: u64,
    /// Ticks that found nothing to do. The rest is how busy the loop was.
    pub idle_ticks: u64,
    /// Requests taken off the intake queue, including the ones rejected on shape or balance.
    pub admitted: u64,
    /// Effects built and put in a batch. Judged minus committed is what consensus still owes.
    pub judged: u64,
    /// Effects applied after commit. This is the number that means "money moved".
    pub committed: u64,
    /// Requests answered as an already-seen transaction rather than applied again.
    pub duplicates: u64,
    /// Requests refused for any reason, from bad shape to insufficient balance.
    pub rejected: u64,
    /// Effects rolled back because consensus refused their batch.
    pub commit_failures: u64,
    /// Proposals handed to consensus. Committed divided by this is the average batch size.
    pub proposed_batches: u64,
    /// Contract-1 violations: an external component returned a lane out of order.
    pub seq_gaps: u64,
    /// Lanes isolated after a gap.
    pub quarantines: u64,
    /// Requests that took no place in a lane's order because their debit is unconstrained. These
    /// never fence, which is what keeps one busy suspense account from serialising.
    pub order_exempt: u64,
    /// Ordering tokens sent because a lane already had a pending reply outstanding. High means
    /// the pending path is holding the fast kinds behind it.
    pub fences: u64,
    /// Linked chains judged and proposed as one unit.
    pub linked_chains_judged: u64,
    /// Linked chains refused whole, for any reason including one bad leg.
    pub linked_chains_rejected: u64,
    /// Requests held back because a chain on their lane had not been judged yet.
    pub lane_gated: u64,
    /// LinkedChains the client never terminated, ended at a batch boundary.
    pub linked_chains_aborted: u64,
    /// Times intake stopped because a backlog filled. Non-zero means the client or the pending
    /// engine is the limit, not the reactor.
    pub intake_pauses: u64,
    /// Dispatches refused by a full external queue and retried later.
    pub dispatch_deferred: u64,
    /// Proposals refused by a full consensus queue and retried later.
    pub propose_deferred: u64,
    /// Requests refused because every work slot was in use.
    pub slot_exhaustion: u64,
    /// Idle overlay entries dropped to stay under the soft limit.
    pub holds_evicted: u64,
    /// Answers from the engine that reflected fewer committed decisions than it had already been handed.
    /// The data check that stands in for the lane's order on a request keeping no place in it: anything
    /// but zero is the engine having reordered its own queue, and the lane is quarantined for it.
    pub stale_answers: u64,
    /// Times the sequencer's own bookkeeping stopped adding up. Anything but zero means the node
    /// sealed its apply path and has to be replaced.
    pub invariant_breaks: u64,
    /// Committed holds the engine could not store: its index was sized for a declared maximum and that
    /// maximum has been passed. The one thing the engine says without being asked, and anything but zero
    /// means the node sealed its apply path — the hold is in the log and no resolution of it can ever be
    /// answered.
    pub holds_not_stored: u64,
    /// Expiry voids the engine asked for and the sequencer admitted: a hold whose retention ran out, whose
    /// remainder has to be released or the pending column it reserved never comes down.
    pub holds_expired: u64,
    /// Expiry voids that could not be admitted — no slot, a quarantined lane, a sealed apply path. Not a
    /// loss: nobody asked for them and the sweep offers them again. Non-zero for long means the sweep is
    /// not keeping up, which is a capacity question rather than a correctness one.
    pub expiry_refused: u64,
    /// State-transition records lost because nobody drained the log stream. Not zero means the
    /// forensics for a gap or a quarantine may be missing.
    pub log_drops: u64,
    /// Records fetched from the engine: one per resolution, bar those of a hold the engine has already
    /// said is not there. Whether the engine answered from memory or had to read the store is its own
    /// number, not one the sequencer can see.
    pub pending_lookups: u64,
    /// Committed decisions handed to the pending engine, by kind. Kept apart because what each costs
    /// the engine is a different question: a create is a record it must store, a reduce is an update
    /// that may cost nothing or a new version, and a remove may free space or write a tombstone.
    /// Adding them up would report a volume as if it were an occupancy.
    pub pending_creates: u64,
    pub pending_reduces: u64,
    pub pending_removes: u64,
    /// Propose-to-commit time summed over batches, and the worst one. This is the part of
    /// latency consensus owns.
    pub commit_wait_nanos: u64,
    pub commit_wait_max_nanos: u64,
}

impl Metrics {
    /// What happened since `base`. A run that measures one stretch of its own life has to divide
    /// counters from that stretch by counters from the same one: a setup phase counted into the
    /// numerator alone reports a ratio that moves with the duration instead of with the ledger.
    pub fn since(&self, base: &Self) -> Self {
        macro_rules! measured {
            ($($counter:ident),* $(,)?) => {
                Self {
                    $($counter: self.$counter.saturating_sub(base.$counter),)*
                    // A maximum, not a total: a stretch cannot be subtracted out of a worst case.
                    commit_wait_max_nanos: self.commit_wait_max_nanos,
                }
            };
        }
        measured!(
            ticks,
            idle_ticks,
            admitted,
            judged,
            committed,
            duplicates,
            rejected,
            commit_failures,
            proposed_batches,
            seq_gaps,
            quarantines,
            order_exempt,
            fences,
            linked_chains_judged,
            linked_chains_rejected,
            lane_gated,
            linked_chains_aborted,
            intake_pauses,
            dispatch_deferred,
            propose_deferred,
            slot_exhaustion,
            holds_evicted,
            stale_answers,
            invariant_breaks,
            holds_not_stored,
            holds_expired,
            expiry_refused,
            log_drops,
            pending_lookups,
            pending_creates,
            pending_reduces,
            pending_removes,
            commit_wait_nanos,
        )
    }
}

/// Reactor-thread time per stage, filled only while `ReactorConfig::profile` is on: it costs a
/// clock read per stage per tick, which is why it is off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageTimes {
    pub backlog: u64,
    pub intake: u64,
    pub judge: u64,
    pub propose: u64,
    pub apply: u64,
}

impl StageTimes {
    pub const fn total(&self) -> u64 {
        self.backlog + self.intake + self.judge + self.propose + self.apply
    }

    pub fn shares(&self) -> [(&'static str, f64); 5] {
        let total = self.total().max(1) as f64;
        [
            ("intake", self.intake as f64 / total),
            ("judge", self.judge as f64 / total),
            ("propose", self.propose as f64 / total),
            ("apply", self.apply as f64 / total),
            ("backlog", self.backlog as f64 / total),
        ]
    }
}
