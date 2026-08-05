use std::collections::BTreeMap;
use std::mem::size_of;
use std::time::Duration;

use ledger_base::ports::LedgerTotals;
use ledger_base::{Amount, Effect, Footprint};
use ledger_sequencer::{Metrics, StageTimes};
use serde::Serialize;

use crate::histogram::Histogram;

/// End-to-end latency as the client sees it: submit to ack, so it includes queueing, the
/// external round trips and consensus.
pub struct LatencySummary {
    pub samples: u64,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    pub max_us: f64,
}

impl From<&Histogram> for LatencySummary {
    fn from(histogram: &Histogram) -> Self {
        let micros = |nanos: u64| nanos as f64 / 1_000.0;
        Self {
            samples: histogram.count(),
            mean_us: micros(histogram.mean_nanos()),
            p50_us: micros(histogram.percentile_nanos(0.50)),
            p90_us: micros(histogram.percentile_nanos(0.90)),
            p99_us: micros(histogram.percentile_nanos(0.99)),
            p999_us: micros(histogram.percentile_nanos(0.999)),
            max_us: micros(histogram.max_nanos()),
        }
    }
}

/// The wire shape of a run, which belongs to this tool rather than to the ledger: the crates it
/// measures stay free of serialisation.
#[derive(Serialize)]
struct JsonReport<'a> {
    workload: &'a str,
    accounts: u64,
    elapsed_s: f64,
    submitted: u64,
    committed: u64,
    duplicates: u64,
    rejected: u64,
    throughput_tps: f64,
    latency_us: JsonLatency,
    batch_latency_us: Option<JsonLatency>,
    seq_gaps: u64,
    quarantined: usize,
    fail_stop: bool,
    batches: u64,
    ticks: u64,
    identities_ok: bool,
    busy_tick_share: f64,
    core_used: f64,
    cpu_per_op_ns: f64,
    commit_wait_us: f64,
    pending_lookups: u64,
    passed: bool,
    rejects: &'a BTreeMap<&'static str, u64>,
}

#[derive(Serialize)]
struct JsonLatency {
    mean: f64,
    p50: f64,
    p90: f64,
    p99: f64,
    p999: f64,
    max: f64,
}

impl From<&LatencySummary> for JsonLatency {
    fn from(latency: &LatencySummary) -> Self {
        Self {
            mean: latency.mean_us,
            p50: latency.p50_us,
            p90: latency.p90_us,
            p99: latency.p99_us,
            p999: latency.p999_us,
            max: latency.max_us,
        }
    }
}

pub struct RunReport {
    pub workload: &'static str,
    pub accounts: u64,
    /// Measured phase only; the funding phase is excluded.
    pub elapsed: Duration,
    /// How long the reactor thread existed, funding and drain included. Stage times cover the
    /// same span, so this is the denominator utilisation has to use.
    pub reactor_elapsed: Duration,
    /// Counted by the client, so it excludes anything the client could not publish.
    pub submitted: u64,
    pub committed: u64,
    pub duplicates: u64,
    pub rejected: u64,
    /// Rejections by category, which is usually the first thing to read when a run looks wrong.
    pub reject_kinds: BTreeMap<&'static str, u64>,
    pub latency: LatencySummary,
    /// Completion of a whole client submission run, which is what a deadline batch is judged on.
    pub batch_latency: Option<LatencySummary>,
    pub metrics: Metrics,
    /// Reactor time per stage. Zero unless the run was profiled.
    pub stages: StageTimes,
    pub profiled: bool,
    pub slo_p999: Option<Duration>,
    /// Both accounting identities, summed over every account after the run.
    pub totals: LedgerTotals,
    /// The propose-time overlay, which must be zero once nothing is in flight.
    pub overlay: Amount,
    pub quarantined: usize,
    pub fail_stop: bool,
    /// Thread placement the reactor actually got, without which the numbers are not comparable.
    pub placement: &'static str,
    /// What each component was holding when the run stopped, in the order a request meets them. Kept
    /// per component because "how much RAM" has no answer and "how much RAM for which structure" does.
    pub footprints: Vec<(&'static str, Footprint)>,
    /// Where the engine's records went and where its reads came from.
    pub pending_traffic: ledger_pending::LogTraffic,
    /// What keeping each lane in seq order cost on top of the reads. Its own field because it is the
    /// engine's orderer's number, not the log's.
    pub order_wait: ledger_pending::OrderWait,
}

impl RunReport {
    /// Reactor core utilisation, from the profiled stage times. A tick can do hundreds of
    /// requests, so this is the only honest saturation measure — and it needs `--cpu`.
    pub fn core_utilisation(&self) -> Option<f64> {
        self.profiled
            .then(|| self.stages.total() as f64 / (self.reactor_elapsed.as_nanos() as f64).max(1.0))
    }

    /// Ticks that found work. Not utilisation: one busy tick may carry hundreds of requests.
    pub fn busy_tick_share(&self) -> f64 {
        let ticks = self.metrics.ticks.max(1) as f64;
        (self.metrics.ticks - self.metrics.idle_ticks) as f64 / ticks
    }

    /// Reactor time per committed transfer. Only meaningful for a profiled run.
    pub fn cpu_per_op_nanos(&self) -> f64 {
        self.stages.total() as f64 / self.metrics.committed.max(1) as f64
    }

    /// Mean propose-to-commit time: the part of latency consensus owns.
    pub fn commit_wait_micros(&self) -> f64 {
        let batches = self.metrics.proposed_batches.max(1) as f64;
        self.metrics.commit_wait_nanos as f64 / batches / 1_000.0
    }

    /// Everything a run has to satisfy to count as passing: the latency target, both accounting
    /// identities, and no contract-1 violation.
    pub fn passed(&self) -> bool {
        let latency_ok = match self.slo_p999 {
            Some(target) => self.latency.p999_us * 1_000.0 <= target.as_nanos() as f64,
            None => true,
        };
        latency_ok
            && self.identities_hold()
            && self.metrics.seq_gaps == 0
            && self.metrics.invariant_breaks == 0
            // An insert the index could not take is a hold the log says exists and the store does not
            // have. The node now seals for it, so `fail_stop` below would fail the run anyway; this is
            // kept because it names the cause, and because it is counted where it happened — the engine
            // — rather than where it was answered.
            && self.pending_traffic.overflowed == 0
            && !self.fail_stop
    }

    /// One block per concern, in the order a reader wants them: what happened, how fast, then what
    /// the ledger and its peers were doing while it did.
    pub fn print_text(&self) {
        println!(
            "ledgerfio: workload={} accounts={} elapsed={:.2}s reactor={}",
            self.workload,
            self.accounts,
            self.elapsed.as_secs_f64(),
            self.placement
        );
        self.print_throughput();
        self.print_latency();
        self.print_reactor();
        self.print_pending();
        self.print_sizing();
        self.print_safety();
        self.print_verdict();
    }

    fn print_throughput(&self) {
        println!(
            "  throughput    {:.0} tx/s (submitted {}, committed {}, duplicates {}, rejected {})",
            self.throughput(),
            self.submitted,
            self.committed,
            self.duplicates,
            self.rejected
        );
    }

    fn print_latency(&self) {
        println!(
            "  latency us    mean={:.0} p50={:.0} p90={:.0} p99={:.0} p99.9={:.0} max={:.0} (n={})",
            self.latency.mean_us,
            self.latency.p50_us,
            self.latency.p90_us,
            self.latency.p99_us,
            self.latency.p999_us,
            self.latency.max_us,
            self.latency.samples
        );
        if let Some(batch) = &self.batch_latency {
            println!(
                "  batch us      p50={:.0} p99={:.0} p99.9={:.0} max={:.0} (n={})",
                batch.p50_us, batch.p99_us, batch.p999_us, batch.max_us, batch.samples
            );
        }
    }

    fn print_reactor(&self) {
        let batches = self.metrics.proposed_batches.max(1);
        println!(
            "  reactor       ticks={} judged={} applied={} batches={} ({:.1} effects/batch)",
            self.metrics.ticks,
            self.metrics.judged,
            self.metrics.committed,
            self.metrics.proposed_batches,
            self.metrics.committed as f64 / batches as f64
        );
        let wait = |nanos: u64| nanos as f64 / 1_000.0;
        match self.core_utilisation() {
            Some(used) => println!(
                "  core          used={:.1}% work ticks={:.1}% consensus wait mean={:.0}us max={:.0}us",
                used * 100.0,
                self.busy_tick_share() * 100.0,
                self.commit_wait_micros(),
                wait(self.metrics.commit_wait_max_nanos)
            ),
            None => println!(
                "  core          work ticks={:.1}% consensus wait mean={:.0}us max={:.0}us (--cpu for utilisation)",
                self.busy_tick_share() * 100.0,
                self.commit_wait_micros(),
                wait(self.metrics.commit_wait_max_nanos)
            ),
        }
        if self.profiled {
            let shares: Vec<String> = self
                .stages
                .shares()
                .iter()
                .map(|(stage, share)| format!("{stage}={:.1}%", share * 100.0))
                .collect();
            println!(
                "  stage cpu     {} ({:.0}ns per committed tx)",
                shares.join(" "),
                self.cpu_per_op_nanos()
            );
            // What a simulation charges its virtual clock: the cost of one unit of each stage's
            // work, measured here rather than guessed there.
            let per = |nanos: u64, count: u64| nanos as f64 / count.max(1) as f64;
            println!(
                "  stage cost    intake={:.0}ns/request judge={:.0}ns/effect propose={:.1}ns/effect apply={:.0}ns/effect",
                per(self.stages.intake, self.metrics.admitted),
                per(self.stages.judge, self.metrics.judged),
                // Serialising into the batch buffer is per effect, not per batch: a batch's cost
                // depends on how much is in it.
                per(self.stages.propose, self.metrics.judged),
                per(self.stages.apply, self.metrics.committed)
            );
        }
    }

    fn print_pending(&self) {
        println!(
            "  pending       lookups={} overlay evicted={}",
            self.metrics.pending_lookups, self.metrics.holds_evicted
        );
        println!(
            "  chains        judged={} rejected={} aborted={} fences={} lane-gated={} exempt={}",
            self.metrics.linked_chains_judged,
            self.metrics.linked_chains_rejected,
            self.metrics.linked_chains_aborted,
            self.metrics.fences,
            self.metrics.lane_gated,
            self.metrics.order_exempt
        );
    }

    /// Two questions, kept apart because one is occupancy and the other is volume.
    ///
    /// **Memory** is occupancy: what a machine had to have. Each part reports the entries live at the
    /// end and the most it ever held, because a structure sized for its mean overflows.
    ///
    /// **Written** is volume: what this workload handed to the storage below, which is not the same as
    /// what that storage keeps. Nothing in this repository writes to disk yet, so there is no
    /// occupancy to report — a log figure here is what the log would append, and the engine's figures
    /// are what it was told to do, not what its layout would cost. Retention is what turns volume into
    /// occupancy, and retention lives in code that does not exist: consensus keeps no snapshot or
    /// compaction, the dedup map has no expiry, and the pending engine has no disk tier.
    fn print_sizing(&self) {
        let mb = |bytes: usize| bytes as f64 / 1e6;
        let total: usize = self.footprints.iter().map(|(_, part)| part.bytes()).sum();
        let exact = self.footprints.iter().all(|(_, part)| part.exact());
        println!(
            "  memory        {:.1}MB held{}",
            mb(total),
            if exact {
                ""
            } else {
                " (hash tables from their bucket count, so approximate)"
            }
        );
        for (component, footprint) in &self.footprints {
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
            // Three to a line: the sequencer alone has eight parts, and one long line hides them.
            for (line, group) in parts.chunks(3).enumerate() {
                let label = if line == 0 {
                    format!("{component}:")
                } else {
                    String::new()
                };
                println!("                {label:15} {}", group.join("  "));
            }
        }
        // A ceiling reached is part of the answer, whoever chose it — including the reserve a bound's
        // own buffer was given, which the open batch overshoots by whatever was dispatched before
        // intake paused.
        let reached: Vec<String> = self
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
            println!("                ceilings reached: {}", reached.join(", "));
        }
        // A property of the design, not of this run, and the reason a total here is not a steady state:
        // the structures that grow fastest have nothing to bound them yet. The engine's blocks are no
        // longer among them — expiry releases the holds that outlive their retention and the records go
        // with them — but a run measured in seconds crosses no day, so a run's own total still shows the
        // unbounded shape rather than the steady state expiry produces.
        println!(
            "                unbounded so far: the dedup map has no expiry and the log no compaction, so \
             both grow with the run; the engine's blocks are bounded by retention, which no run this \
             short reaches"
        );
        // The log is append-only and this design has no snapshot or compaction, so what it appended is
        // what it would occupy. Priced at the effect's in-memory size because there is no wire format
        // yet: when there is one, only this multiplier changes.
        let effect_bytes = size_of::<Effect>();
        let appended = self.metrics.committed as usize * effect_bytes;
        let seconds = self.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "  log written   {:.1}MB over the run ({:.0}MB/s), {} committed effects at {}B unencoded",
            mb(appended),
            mb(appended) / seconds,
            self.metrics.committed,
            effect_bytes
        );
        if self.metrics.commit_failures > 0 {
            println!(
                "                {} effects were in batches consensus refused — whether those reached \
                 disk is the log implementation's answer, not this one's",
                self.metrics.commit_failures
            );
        }
        // By kind, never summed: a create is a record, a reduce appends a version, and a remove writes
        // nothing at all. One total would read as occupancy, which is the line below.
        println!(
            "  engine told   create={} reduce={} remove={}",
            self.metrics.pending_creates,
            self.metrics.pending_reduces,
            self.metrics.pending_removes
        );
        // What the writeback buffer is for, as a number. A record resolved before its block is
        // compacted never reaches the store, so what the store has to hold is holds *alive* rather than
        // holds created — the difference the source design's capacity estimate rests on, and the one
        // its own inputs disagree about. Reads are split the same way: only the last kind is an IO once
        // the store is a disk.
        let traffic = self.pending_traffic;
        let share = |part: u64, whole: u64| {
            if whole == 0 {
                0.0
            } else {
                part as f64 / whole as f64 * 100.0
            }
        };
        println!(
            "  engine log    appended={} ({:.1}MB) died in buffer={} ({:.0}%) carried on={} \
             left memory={}",
            traffic.appended,
            mb(traffic.appended as usize * ledger_pending::RECORD_BYTES),
            traffic.died_in_buffer,
            share(traffic.died_in_buffer, traffic.appended),
            traffic.flushed,
            traffic.left_memory
        );
        // Where each read was answered from, which is what says whether either window is earning its
        // size. `unwritten` is the flush window's, `resident` is the residency window's, and only the
        // last kind is an IO once the store is a disk.
        println!(
            "  engine reads  unwritten={} resident={} store={} ({:.1}% of reads, apply {}, peak depth {})",
            traffic.buffer_reads,
            traffic.resident_reads,
            traffic.store_reads,
            share(
                traffic.store_reads,
                traffic.buffer_reads + traffic.resident_reads + traffic.store_reads
            ),
            traffic.apply_store_reads,
            traffic.inflight_peak
        );
        // A read that finished on time and then waited for an earlier read on its lane is a speed problem
        // no per-read bound covers, and it is the product — lane depth times read latency — rather than
        // either term. Printed only when a lane ever waited, because zero is the answer whenever reads
        // are answered from memory, and a line of zeroes reads as if it were not measured.
        let wait = self.order_wait;
        if wait.released > 0 {
            println!(
                "  engine order  {} of {} replies arrived behind their lane ({:.1}%), mean {:.0}us \
                 worst {:.0}us, deepest lane {} | delivery mean {:.0}us",
                wait.held_for_order,
                wait.released,
                share(wait.held_for_order, wait.released),
                wait.order_nanos as f64 / wait.held_for_order.max(1) as f64 / 1_000.0,
                wait.order_worst_nanos as f64 / 1_000.0,
                wait.deepest_lane,
                wait.delivery_nanos as f64 / wait.released.max(1) as f64 / 1_000.0
            );
        }
        // The table is sized once and never grows, so these are what say whether the sizing still holds.
        // Both move before an insert can fail: the load factor against what it was sized for, and the
        // longest cascade against the cap that bounds an insert.
        let load = if traffic.index_slots == 0 {
            0.0
        } else {
            traffic.index_live as f64 / traffic.index_slots as f64
        };
        println!(
            "  engine index  {} of {} slots (load {:.3} of {:.2} target)  worst cascade {} of {}  \
             ambiguous={} overflowed={}",
            traffic.index_live,
            traffic.index_slots,
            load,
            ledger_pending::LOAD_TARGET,
            traffic.worst_cascade,
            128,
            traffic.ambiguous,
            traffic.overflowed
        );
    }

    fn print_safety(&self) {
        println!(
            "  contract-1    seq gaps={} quarantined lanes={} fail-stop={} log drops={}",
            self.metrics.seq_gaps, self.quarantined, self.fail_stop, self.metrics.log_drops
        );
        if self.metrics.invariant_breaks > 0 {
            println!(
                "  BOOKKEEPING   {} invariant breaks: the node sealed its apply path",
                self.metrics.invariant_breaks
            );
        }
        // The days behind belong beside the refusals, because they are cause and effect: a void with no
        // room is offered again next round, and a day that keeps being offered again is a day not emptied.
        // One is ordinary; more than the configured grace is the throttle behind by longer than the index
        // was sized to allow, which ends in the seal below rather than in late deletion.
        let behind = self.pending_traffic.days_behind;
        if self.metrics.holds_expired + self.metrics.expiry_refused > 0 || behind > 0 {
            println!(
                "  retention     {} holds released for outliving it, {} refused and {} dropped by a full queue \
                 (both offered again), {} expired days still to empty",
                self.metrics.holds_expired,
                self.metrics.expiry_refused,
                self.metrics.expiry_dropped,
                behind
            );
        }
        if self.metrics.holds_not_stored > 0 {
            println!(
                "  SEALED        {} committed holds the engine could not store: its index was sized \
                 for a declared maximum this run passed, so the node stopped applying. Raise \
                 --daily-arrivals or --index-budget.",
                self.metrics.holds_not_stored
            );
        }
        println!(
            "  backpressure  intake pauses={} dispatch deferred={} propose deferred={} slots exhausted={} commit failures={}",
            self.metrics.intake_pauses,
            self.metrics.dispatch_deferred,
            self.metrics.propose_deferred,
            self.metrics.slot_exhaustion,
            self.metrics.commit_failures
        );
    }

    /// Whether the run is one anyone should believe, and why it might not be.
    fn print_verdict(&self) {
        println!(
            "  identities    posted {} == {}, pending {} == {}, overlay {} [{}]",
            self.totals.debits_posted,
            self.totals.credits_posted,
            self.totals.debits_pending,
            self.totals.credits_pending,
            self.overlay,
            if self.identities_hold() {
                "ok"
            } else {
                "BROKEN"
            }
        );
        if let Some(target) = self.slo_p999 {
            println!(
                "  slo           p99.9 {:.0}us <= {:.0}us [{}]",
                self.latency.p999_us,
                target.as_nanos() as f64 / 1_000.0,
                if self.passed() { "pass" } else { "FAIL" }
            );
        }
        if self.reject_kinds.is_empty() {
            return;
        }
        let kinds: Vec<String> = self
            .reject_kinds
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect();
        println!("  rejects       {}", kinds.join(" "));
    }

    /// One line per sweep step, so a knob's effect reads down a column.
    pub fn print_row_header(knob: &str) {
        println!(
            "{:<14} {:>10} {:>8} {:>9} {:>9} {:>6} {:>6}",
            knob, "tx/s", "p50us", "p99.9us", "ns/tx", "gaps", "result"
        );
    }

    pub fn print_row(&self, value: &str) {
        let per_op = match self.profiled {
            true => format!("{:.0}", self.cpu_per_op_nanos()),
            false => "-".to_owned(),
        };
        println!(
            "{:<14} {:>10.0} {:>8.0} {:>9.0} {:>9} {:>6} {:>6}",
            value,
            self.throughput(),
            self.latency.p50_us,
            self.latency.p999_us,
            per_op,
            self.metrics.seq_gaps,
            if self.passed() { "pass" } else { "FAIL" }
        );
    }

    pub fn print_json(&self) {
        println!("{}", self.json());
    }

    /// The machine-readable form of the same run. Shaped by a struct rather than a format string,
    /// so a new field cannot land in the wrong place or lose a comma.
    fn json(&self) -> String {
        let line = JsonReport {
            workload: self.workload,
            accounts: self.accounts,
            elapsed_s: self.elapsed.as_secs_f64(),
            submitted: self.submitted,
            committed: self.committed,
            duplicates: self.duplicates,
            rejected: self.rejected,
            throughput_tps: self.throughput(),
            latency_us: JsonLatency::from(&self.latency),
            batch_latency_us: self.batch_latency.as_ref().map(JsonLatency::from),
            seq_gaps: self.metrics.seq_gaps,
            quarantined: self.quarantined,
            fail_stop: self.fail_stop,
            batches: self.metrics.proposed_batches,
            ticks: self.metrics.ticks,
            identities_ok: self.identities_hold(),
            busy_tick_share: self.busy_tick_share(),
            core_used: self.core_utilisation().unwrap_or(0.0),
            cpu_per_op_ns: self.cpu_per_op_nanos(),
            commit_wait_us: self.commit_wait_micros(),
            pending_lookups: self.metrics.pending_lookups,
            passed: self.passed(),
            rejects: &self.reject_kinds,
        };
        serde_json::to_string(&line).expect("a report serialises")
    }

    pub fn throughput(&self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        self.committed as f64 / seconds
    }

    fn identities_hold(&self) -> bool {
        self.totals.debits_posted == self.totals.credits_posted
            && self.totals.debits_pending == self.totals.credits_pending
            && self.overlay == 0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use ledger_base::ports::LedgerTotals;
    use ledger_sequencer::{Metrics, StageTimes};

    use super::*;

    fn report() -> RunReport {
        let mut histogram = Histogram::new();
        for value in 1..=1_000u64 {
            histogram.record(value * 1_000);
        }
        RunReport {
            workload: "hold-settle",
            accounts: 100_000,
            elapsed: Duration::from_millis(2_500),
            reactor_elapsed: Duration::from_millis(3_000),
            submitted: 1_000,
            committed: 990,
            duplicates: 4,
            rejected: 6,
            reject_kinds: BTreeMap::from([("InsufficientBalance", 6u64)]),
            latency: LatencySummary::from(&histogram),
            batch_latency: Some(LatencySummary::from(&histogram)),
            metrics: Metrics {
                ticks: 42,
                seq_gaps: 0,
                ..Metrics::default()
            },
            stages: StageTimes::default(),
            profiled: false,
            slo_p999: None,
            totals: LedgerTotals::default(),
            overlay: 0,
            quarantined: 0,
            fail_stop: false,
            placement: "performance-qos",
            footprints: Vec::new(),
            pending_traffic: ledger_pending::LogTraffic::default(),
            order_wait: ledger_pending::OrderWait::default(),
        }
    }

    /// The JSON line is a machine interface: it has to parse, and the keys another tool reads have to
    /// be there with the right shape. A hand-built line can lose a comma without anyone noticing,
    /// which is what this catches.
    #[test]
    fn the_json_line_parses_and_carries_the_keys_a_tool_reads() {
        let report = report();
        let parsed: serde_json::Value =
            serde_json::from_str(&report.json()).expect("the report must be valid JSON");

        assert_eq!(parsed["workload"], "hold-settle");
        assert_eq!(parsed["accounts"], 100_000);
        assert_eq!(parsed["committed"], 990);
        assert_eq!(parsed["duplicates"], 4);
        assert_eq!(parsed["identities_ok"], true);
        assert_eq!(parsed["passed"], true);
        assert_eq!(parsed["rejects"]["InsufficientBalance"], 6);

        let latency = &parsed["latency_us"];
        for key in ["mean", "p50", "p90", "p99", "p999", "max"] {
            assert!(latency[key].is_number(), "latency_us.{key} is missing");
        }
        assert!(
            latency["p50"].as_f64().expect("p50") <= latency["p999"].as_f64().expect("p999"),
            "the quantiles are out of order"
        );
        assert_eq!(
            parsed["throughput_tps"]
                .as_f64()
                .expect("throughput")
                .round(),
            (990.0 / 2.5f64).round()
        );
    }
}
