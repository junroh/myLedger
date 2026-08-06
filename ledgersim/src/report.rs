//! What a run prints. Separate from the modes for the same reason the load driver's report is: the
//! numbers are one thing and deciding what to run is another, and both grew until the file had to be
//! read twice.

use std::mem::size_of;

use ledger_base::Effect;

use crate::sim::{Failure, Plan, Prediction, Report};

/// The full capacity block: what the run did, the evidence, and which limit it found.
pub fn prediction(plan: &Plan, prediction: &Prediction, verdict: Option<Verdict>) {
    let load = if plan.rate > 0 {
        format!(", offered {}/s", plan.rate)
    } else {
        ", saturated".to_owned()
    };
    println!(
        "ledgersim capacity: {} accounts, pending engine {}us+{}us tail at {}/s, consensus {}us, client qd {}{}",
        plan.accounts,
        plan.pending_nanos / 1_000,
        plan.pending_tail_nanos / 1_000,
        plan.pending_rate,
        plan.raft_nanos / 1_000,
        plan.queue_depth,
        load
    );
    if plan.cost_percent != 100 {
        println!(
            "  costs          every stage at {}% of what this machine measured — an assumption about a \
             core, not a derivation from one",
            plan.cost_percent
        );
    }
    println!(
        "  throughput     {:.0} tx/s over {:.1}ms of virtual time, {:.0} submissions/s offered",
        prediction.throughput(),
        prediction.virtual_nanos as f64 / 1e6,
        prediction.offered()
    );
    println!(
        "  answered       {} committed, {} refused ({} for want of a slot), {} duplicate",
        prediction.committed,
        prediction.metrics.rejected,
        prediction.overloaded,
        prediction.metrics.duplicates
    );
    // What the core paid for per effect that actually landed: the traffic here is messier than a
    // benchmark's, so admitted work per committed effect is part of the answer, not noise.
    println!(
        "  work           {} admitted per committed effect, {:.0}ns charged per committed effect",
        format_args!(
            "{:.2}",
            prediction.metrics.admitted as f64 / prediction.committed.max(1) as f64
        ),
        prediction.charge.total() as f64 / prediction.committed.max(1) as f64
    );
    evidence(plan, prediction);
    sizing(plan, prediction);
    limit(plan, prediction);
    if let Some(verdict) = verdict {
        println!(
            "  slo            p99.9 {:.0}us <= {:.0}us [{}]",
            prediction.latency_us[2],
            verdict.target_nanos as f64 / 1_000.0,
            if verdict.held { "pass" } else { "FAIL" }
        );
    }
    // What one round trip carried. A batch far short of its policy size means the round trip is being
    // paid for too little work — which is offered concurrency, not a batching setting.
    println!(
        "  batches        {} proposed, {:.0} effects each",
        prediction.metrics.proposed_batches,
        prediction.metrics.judged as f64 / prediction.metrics.proposed_batches.max(1) as f64
    );
    println!(
        "  estimate       the logic is the real reactor's. The per-stage costs are measured by"
    );
    println!(
        "                 `ledgerfio run --cpu` at one working set on one machine — apply alone"
    );
    println!(
        "                 costs 3.6ns over a thousand accounts and 21.2ns over eight million, so"
    );
    println!("                 another machine or another working set means measuring again and");
    println!("                 passing --cost-*. The components' latencies are declared inputs.");
}

/// The numbers any answer has to show its work with, whichever mode asked for it.
pub fn evidence(plan: &Plan, prediction: &Prediction) {
    let [p50, p99, p999, worst] = prediction.latency_us;
    println!(
        "  latency us     p50={p50:.0} p99={p99:.0} p99.9={p999:.0} worst={worst:.0} mean={:.0} (committed only)",
        prediction.mean_us
    );
    // The sample count behind that tail. A closed loop always ends with its queue depth outstanding,
    // so this is under 100% in a healthy run too; what matters is whether that depth turned over at
    // all, because a tail drawn from one batch still in flight gets better the slower the component is.
    println!(
        "  completed      {} of {} submissions answered ({:.1}%), {} outstanding at the end{}",
        prediction.answered,
        prediction.submitted,
        prediction.completion() * 100.0,
        prediction.outstanding,
        if prediction.queue_depth_turned_over() {
            ""
        } else {
            " — fewer answers than requests still outstanding, so the quantiles above are not a \
             measurement"
        }
    );
    let charge = prediction.charge;
    let share = |part: u64| part as f64 / charge.total().max(1) as f64 * 100.0;
    println!(
        "  core           used={:.1}% of the clock; whatever is left of it waited on a component or on load",
        prediction.core_used() * 100.0
    );
    // Where the core went, which is what says how a saturated one could be relieved: everything but
    // apply is work a worker could take.
    println!(
        "  core split     intake {:.0}% judge {:.0}% propose {:.0}% apply {:.0}% bare ticks {:.0}%",
        share(charge.intake_ns),
        share(charge.judge_ns),
        share(charge.propose_ns),
        share(charge.apply_ns),
        share(charge.tick_ns)
    );
    // An arrival rate, not a served one: every resolution is here, because the record it is judged by
    // is the engine's. What the engine's own memory saves is the IO below this, not a command. Both
    // numbers are computed, not assumed: the rate follows the traffic, and the concurrency follows from
    // it and the latency.
    let asked = rate_of(prediction.pending_commands, prediction);
    let in_flight = asked * plan.pending_nanos as f64 / 1e9;
    if plan.pending_rate > 0 {
        println!(
            "  pending engine asked {asked:.0} commands/s of {} it can answer ({:.0}%), {in_flight:.0} in flight of {} before queueing, mean wait {:.2}ms",
            plan.pending_rate,
            asked / plan.pending_rate as f64 * 100.0,
            plan.pending_concurrency(),
            prediction.pending_queue_us / 1_000.0
        );
    } else {
        println!(
            "  pending engine asked {asked:.0} commands/s, needing {in_flight:.0} in flight at the {}us it \
             answers in (no ceiling set)",
            plan.pending_nanos / 1_000
        );
    }
    // What the lane's order cost on top of the device. A read that finished and then waited for an
    // earlier read on its lane is the risk no per-read bound covers, so it gets its own line — next to
    // the one mitigation, which is that an unconstrained debit takes no place in a lane's order at all.
    if prediction.order_wait_deepest > 0 {
        println!(
            "  order wait     mean {:.2}ms worst {:.2}ms, {} results deep behind a lane head, {:.0}% \
             of requests order-exempt",
            prediction.order_wait_us / 1_000.0,
            prediction.order_wait_worst_us / 1_000.0,
            prediction.order_wait_deepest,
            prediction.metrics.order_exempt as f64 / prediction.metrics.admitted.max(1) as f64 * 100.0
        );
    }
    let lookups = prediction.metrics.pending_lookups;
    println!(
        "  pending path   lookups={lookups} store reads={} ({:.0}% of them cost an IO) \
         overlay evicted={} fences={}",
        prediction.store_reads,
        prediction.store_reads as f64 / lookups.max(1) as f64 * 100.0,
        prediction.metrics.holds_evicted,
        prediction.metrics.fences
    );
    // Only where a day passed. Silence beats a line of zeroes that reads as a measurement: a run with no
    // `--day-ms` has not asked the question, which is not the same as answering nothing fell behind.
    if prediction.metrics.holds_expired + prediction.metrics.expiry_refused > 0
        || prediction.days_behind_worst > 0
    {
        println!(
            "  retention      expiry admitted={} refused={} dropped={} (both offered again next \
             round), worst {} days behind",
            prediction.metrics.holds_expired,
            prediction.metrics.expiry_refused,
            prediction.metrics.expiry_dropped,
            prediction.days_behind_worst
        );
    }
}

/// What this workload would occupy, and what it would push down. The two are different questions and
/// this tool can only half-answer each, so it says which half.
///
/// **Occupancy** it can answer for the reactor and the account store, because those are the ledger's
/// own code running here. It cannot answer it for the pending engine: the store behind the port is a
/// stand-in's hash map, not the index the engine will have, and the retention that decides its steady
/// size — TTL partitions, expiry — is not written. Sizing that structure is the Python simulator's
/// question until it exists.
///
/// **Volume** it can answer in full, because volume is a count of decisions and the decisions are the
/// real reactor's. What the storage below keeps of it is that storage's own answer.
fn sizing(plan: &Plan, prediction: &Prediction) {
    let mb = |bytes: usize| bytes as f64 / 1e6;
    let total: usize = prediction
        .footprints
        .iter()
        .map(|(_, part)| part.bytes())
        .sum();
    println!(
        "  memory         {:.1}MB in the reactor and the account store (hash tables approximate)",
        mb(total)
    );
    for (component, footprint) in &prediction.footprints {
        let parts: Vec<String> = footprint
            .parts()
            .iter()
            .map(|part| {
                format!(
                    "{}={:.1}MB peak {}",
                    part.name,
                    mb(part.bytes),
                    part.peak_entries
                )
            })
            .collect();
        for (line, group) in parts.chunks(3).enumerate() {
            let label = if line == 0 {
                format!("{component}:")
            } else {
                String::new()
            };
            println!("                {label:11} {}", group.join("  "));
        }
    }
    // A ceiling reached is part of the answer, whoever chose it. Said by name rather than left for a
    // reader to divide two numbers out of the lines above.
    let reached: Vec<String> = prediction
        .footprints
        .iter()
        .flat_map(|(_, footprint)| footprint.parts())
        .filter(|part| part.fill() >= 0.8)
        .map(|part| {
            format!(
                "{} at {:.0}% of {}",
                part.name,
                part.fill() * 100.0,
                part.capacity
            )
        })
        .collect();
    if !reached.is_empty() {
        println!(
            "                ceilings reached: {} — a run that filled one is partly an answer about that \
             ceiling rather than about the hardware",
            reached.join(", ")
        );
    }
    // Said in the output and not only in this function's doc: a total that silently leaves out the two
    // largest structures would be read as the whole answer.
    println!(
        "                not sized here: the pending engine's store and the idem map are stand-ins in \
         this tool, so their size would be a stand-in's — `ledgerfio` measures the real ones"
    );
    // Append-only, no snapshot or compaction in this design, so what was appended is what it holds.
    // Priced at the effect's in-memory size because there is no wire format yet: when there is one,
    // only this multiplier changes.
    let effect_bytes = size_of::<Effect>();
    let appended = prediction.metrics.committed as usize * effect_bytes;
    let seconds = prediction.virtual_nanos as f64 / 1e9;
    println!(
        "  log written    {:.1}MB of virtual time ({:.0}MB/s), {} committed effects at {}B unencoded",
        mb(appended),
        mb(appended) / seconds.max(f64::MIN_POSITIVE),
        prediction.metrics.committed,
        effect_bytes
    );
    // By kind and never summed: a create is a record the engine must keep, a reduce appends a new
    // version, and a remove frees nothing until a segment expires. One total would read as an
    // occupancy this tool has no way to predict. Priced at the record the engine actually writes, not
    // at the struct the port passes around — those were the same size by accident once, and a figure
    // that moves when a port field is added was never measuring the store.
    println!(
        "  engine told    create={} reduce={} remove={} ({:.1}MB of records written) — at {} accounts, \
         what the engine keeps of that is its own layout's answer",
        prediction.metrics.pending_creates,
        prediction.metrics.pending_reduces,
        prediction.metrics.pending_removes,
        mb((prediction.metrics.pending_creates + prediction.metrics.pending_reduces) as usize
            * ledger_pending::RECORD_BYTES),
        plan.accounts
    );
}

/// Which limit the run found. A saturated closed loop measures the smallest of them, and saying which
/// is the difference between a prediction and a number.
fn limit(plan: &Plan, prediction: &Prediction) {
    if plan.rate > 0 {
        return;
    }
    // The core first, because when it is the limit none of the others are: the stage split is what
    // says how the ceiling could be raised, and `apply` is the one stage that cannot leave this core.
    if prediction.core_used() >= 0.9 {
        let in_order = prediction.charge.in_order_share();
        println!(
            "  limit          the core: {:.0}% of the clock, and {:.0}% of that went to apply, which \
             applies in commit order and cannot be moved off this core. {}",
            prediction.core_used() * 100.0,
            in_order * 100.0,
            if in_order >= 0.5 {
                "Raising this ceiling means sharding, not a worker."
            } else {
                "The rest is pre-consensus work a worker could take before sharding is needed."
            }
        );
        return;
    }
    // Not "is the pending engine near its rate" — arrivals are bursty, so a service at 87% of its
    // rate still queues for tens of milliseconds. What decides it is whether waiting for it is a real
    // share of what a request waits for at all.
    if plan.pending_rate > 0 && prediction.pending_queue_us > prediction.mean_us * 0.2 {
        println!(
            "  limit          the pending engine: asked for {:.0} commands/s of {} it can answer, so \
             commands wait {:.0}ms for it. A faster engine or a higher resident hit ratio, not a bigger \
             queue depth.",
            rate_of(prediction.pending_commands, prediction),
            plan.pending_rate,
            prediction.pending_queue_us / 1_000.0
        );
    } else if prediction.overloaded > 0 {
        println!(
            "  limit          the ledger's slots: it refused {} requests rather than queue them",
            prediction.overloaded
        );
    } else {
        println!(
            "  limit          the client, not the ledger: at qd {} and a {:.1}ms mean the client cannot \
             have more than {:.0} tx/s outstanding, so the core sat idle {:.0}% of the time. This run \
             says nothing about the ledger's ceiling — raise --qd to ask about that.",
            plan.queue_depth,
            prediction.mean_us / 1_000.0,
            plan.queue_depth as f64 * 1e6 / prediction.mean_us.max(1.0),
            (1.0 - prediction.core_used()) * 100.0
        );
    }
}

pub fn rate_of(count: u64, prediction: &Prediction) -> f64 {
    count as f64 / (prediction.virtual_nanos as f64 / 1e9)
}

/// A target the run was asked to hold, and whether it did. Present only when one was named, because a
/// run with no target has nothing to pass or fail.
#[derive(Clone, Copy)]
pub struct Verdict {
    pub target_nanos: u64,
    pub held: bool,
}

/// One line per sweep step, so a knob's effect reads down a column instead of across shell history.
/// `answered` is here rather than in a footnote: it is what says whether the tail beside it counts.
pub fn sweep_header(knob: &str) {
    println!(
        "{:<16} {:>10} {:>8} {:>9} {:>9} {:>9} {:>7}",
        knob, "tx/s", "p50us", "p99.9us", "answered", "ns/commit", "result"
    );
}

pub fn sweep_row(value: &str, prediction: &Prediction, held: bool) {
    println!(
        "{:<16} {:>10.0} {:>8.0} {:>9.0} {:>9} {:>9.0} {:>7}",
        value,
        prediction.throughput(),
        prediction.latency_us[0],
        prediction.latency_us[2],
        prediction.answered,
        prediction.charge.total() as f64 / prediction.committed.max(1) as f64,
        if held { "pass" } else { "FAIL" }
    );
}

pub fn seed(report: &Report) {
    let metrics = report.metrics;
    println!(
        "seed {:>4}: submitted {:>7} answered {:>7} committed {:>7} gaps {:>3} chains {:>5} fences {:>6} {}",
        report.seed,
        report.submitted,
        report.answered,
        metrics.committed,
        metrics.seq_gaps,
        metrics.linked_chains_judged,
        metrics.fences,
        match (report.halted, report.funded) {
            (true, false) => "halted before funding finished",
            (true, true) => "halted",
            _ => "",
        }
    );
}

pub fn failure(failure: &Failure) -> ! {
    eprintln!(
        "ledgersim: seed {} broke {:?} at step {}",
        failure.seed, failure.broken, failure.step
    );
    eprintln!("  timings: {:?}", failure.timings);
    eprintln!("  faults: {:?}", failure.faults);
    eprintln!("  reproduce with: ledgersim check --seed {}", failure.seed);
    std::process::exit(1)
}

/// What a sweep actually reached. "No invariant broke" only means something if the states that break
/// invariants were visited, so the sweep says which ones it saw.
#[derive(Default)]
pub struct Coverage {
    committed: u64,
    rejected: u64,
    duplicates: u64,
    chains: u64,
    chains_rejected: u64,
    fences: u64,
    exempt: u64,
    exempt_lookups: u64,
    overflowed: u64,
    /// Committed holds the engine could not store, as the *ledger* saw them. Beside `overflowed`, which
    /// is the same event counted from outside, so a sweep says whether every one of them reached the
    /// node that has to stop for it.
    not_stored: u64,
    store_failures: u64,
    /// Holds whose retention ran out: what the engine offered, what the sequencer admitted, and what it
    /// could not take yet. The third is not a loss — nobody asked for those and the sweep offers them
    /// again — but it is what says the expiry rate is short.
    expiries_offered: u64,
    expired: u64,
    expiry_refused: u64,
    /// Voids the sequencer never offered a slot to at all: the lane still owed a judgment, or the parking
    /// queue was full. Beside `expiry_refused`, which is one that reached the judge's rules — the two say
    /// different things about where the sweep is being held up.
    expiry_dropped: u64,
    /// The furthest behind any seed's sweep fell, in days — a max rather than a sum, because it is a level
    /// and seeds do not add up. One is ordinary; more than a seed's grace is where late deletion stops
    /// being the safe direction and becomes an index that outgrows its declared maximum.
    days_behind_worst: u64,
    store_reads: u64,
    store_faults: u64,
    store_corruptions: u64,
    stale_answers: u64,
    lookups: u64,
    evicted: u64,
    gaps: u64,
    quarantines: u64,
    commit_failures: u64,
    intake_pauses: u64,
}

impl Coverage {
    pub fn add(&mut self, report: &Report) {
        let m = report.metrics;
        self.committed += m.committed;
        self.rejected += m.rejected;
        self.duplicates += m.duplicates;
        self.chains += m.linked_chains_judged;
        self.chains_rejected += m.linked_chains_rejected;
        self.fences += m.fences;
        self.exempt += m.order_exempt;
        self.exempt_lookups += report.exempt_lookups;
        self.overflowed += report.overflowed;
        self.not_stored += m.holds_not_stored;
        self.store_failures += m.store_failures;
        self.expiries_offered += report.expiries_offered;
        self.expired += m.holds_expired;
        self.expiry_refused += m.expiry_refused;
        self.expiry_dropped += m.expiry_dropped;
        self.days_behind_worst = self.days_behind_worst.max(report.days_behind_worst);
        self.store_reads += report.store_reads;
        self.store_faults += report.store_faults;
        self.store_corruptions += report.store_corruptions;
        self.stale_answers += report.metrics.stale_answers;
        self.lookups += m.pending_lookups;
        self.evicted += m.holds_evicted;
        self.gaps += m.seq_gaps;
        self.quarantines += m.quarantines;
        self.commit_failures += m.commit_failures;
        self.intake_pauses += m.intake_pauses;
    }

    pub fn print(&self) {
        println!(
            "  reached: committed {} rejected {} duplicates {} chains {}/{} rejected",
            self.committed, self.rejected, self.duplicates, self.chains, self.chains_rejected
        );
        println!(
            "           fences {} exempt {} (lookups {}) lookups {} overlay evicted {} \
             store reads {} index overflows {} (sealed {})",
            self.fences,
            self.exempt,
            self.exempt_lookups,
            self.lookups,
            self.evicted,
            self.store_reads,
            self.overflowed,
            self.not_stored
        );
        println!(
            "           store refused {} answered {} blocks wrongly (sealed {})",
            self.store_faults, self.store_corruptions, self.store_failures
        );
        println!(
            "           expiry offered {} admitted {} refused {} dropped {}, worst {} \
             days behind",
            self.expiries_offered,
            self.expired,
            self.expiry_refused,
            self.expiry_dropped,
            self.days_behind_worst
        );
        println!(
            "           seq gaps {} stale answers {} quarantines {} refused commits {} \
             intake pauses {}",
            self.gaps,
            self.stale_answers,
            self.quarantines,
            self.commit_failures,
            self.intake_pauses
        );
    }
}
