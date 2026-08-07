//! One seed, one run, in either of two modes that share everything but the clock.
//!
//! **check** advances the clock by a fixed step and asks the ledger's own audit after every one: the
//! question is whether any interleaving breaks an invariant, and the answer owes nothing to how fast
//! this machine is.
//!
//! **capacity** advances the clock by what the work would have cost, so the virtual time a run takes
//! is a prediction. The control flow is the real reactor's — batching, fences, chains, backpressure —
//! and only the per-stage costs are a model, measured by `ledgerfio run --cpu` rather than guessed.
//! That is the difference from a simulator that reimplements the sequencer: here the logic is the
//! ledger's, and only the numbers are estimated.
//!
//! Nothing here reads the wall clock or starts a thread, so a seed is the whole story: a failure
//! reports the seed and the step, and that seed produces the same failure again.

use std::collections::VecDeque;

use hdrhistogram::Histogram;
use ledger_account::MemoryAccounts;
use ledger_base::ports::AccountFlags;
use ledger_base::{
    channel, AccountId, Ack, AckOutcome, Consumer, Footprint, FxHashMap, LedgerError, ManualClock,
    Prng, Producer, Request, TxId,
};
use ledger_sequencer::{BatchPolicy, Broken, Capacity, Metrics, Reactor, ReactorConfig, Transport};

use crate::fakes::{Faults, IdemFake, PendingFake, RaftFake, Timings};
use crate::workload::{Traffic, EXTERNAL, FIRST_USER, FUNDING, LEDGER};

/// One virtual step in check mode. Its size only matters against the component delays.
pub const STEP_NANOS: u64 = 1_000;

/// How long a partial batch waits in check mode, where the clock moves in fixed steps and a long
/// linger would mean most steps do nothing. Capacity uses the real policy's value, from the plan.
const CHECK_LINGER_NANOS: u64 = STEP_NANOS * 2;

/// What one unit of each stage's work costs. The defaults are what `ledgerfio run --cpu` measured on
/// one Apple M-series core; another machine means measuring again rather than believing these.
#[derive(Debug, Clone, Copy)]
pub struct Costs {
    pub intake_ns: f64,
    pub judge_ns: f64,
    /// Serialising one effect into the batch buffer, not one batch: a batch costs what is in it.
    pub propose_ns: f64,
    pub apply_ns: f64,
    /// The shortest a tick can take. A tick that polled a reply and moved a request between stages
    /// charges none of the stages above, and it still ran: this is that tick's cost, and it is the
    /// core's own time rather than time spent waiting.
    pub tick_ns: f64,
}

impl Costs {
    /// Every stage scaled by the same percentage. For a machine nobody here can run: the honest form
    /// of that question is a bracket, not a derivation. What the stages cost relative to each other was
    /// measured; how much slower the whole core is, is the assumption, and it is stated as one number
    /// instead of hidden in a formula.
    pub fn scaled(self, percent: u64) -> Self {
        let factor = percent as f64 / 100.0;
        Self {
            intake_ns: self.intake_ns * factor,
            judge_ns: self.judge_ns * factor,
            propose_ns: self.propose_ns * factor,
            apply_ns: self.apply_ns * factor,
            tick_ns: self.tick_ns * factor,
        }
    }
}

impl Default for Costs {
    fn default() -> Self {
        // Measured with `ledgerfio run --workload hold-settle --cpu` on one Apple M-series core.
        Self {
            intake_ns: 181.0,
            judge_ns: 93.0,
            propose_ns: 5.0,
            apply_ns: 135.0,
            tick_ns: 50.0,
        }
    }
}

/// What the core spent, and on which stage. The split is what says how a saturated run could be
/// unsaturated: `apply` is the one stage that cannot be moved off this core — it applies in commit
/// order — so a run that spends itself there can only be sharded, while the rest is work a worker
/// could take.
#[derive(Debug, Default, Clone, Copy)]
pub struct StageCharge {
    pub intake_ns: u64,
    pub judge_ns: u64,
    pub propose_ns: u64,
    pub apply_ns: u64,
    /// Ticks that got somewhere without finishing a stage, at the tick floor.
    pub tick_ns: u64,
}

impl StageCharge {
    pub fn total(&self) -> u64 {
        self.intake_ns + self.judge_ns + self.propose_ns + self.apply_ns + self.tick_ns
    }

    /// What share of the core went to the stage that only sharding can relieve.
    pub fn in_order_share(&self) -> f64 {
        self.apply_ns as f64 / self.total().max(1) as f64
    }

    pub fn since(&self, base: &Self) -> Self {
        Self {
            intake_ns: self.intake_ns - base.intake_ns,
            judge_ns: self.judge_ns - base.judge_ns,
            propose_ns: self.propose_ns - base.propose_ns,
            apply_ns: self.apply_ns - base.apply_ns,
            tick_ns: self.tick_ns - base.tick_ns,
        }
    }
}

/// Which question the run is answering. Everything that differs per tick is decided here rather than
/// by a flag at each call site: what the clock does, whether latency is recorded, and how often the
/// ledger is asked to audit itself.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// A fixed step, no latency recorded, and an audit after every one. The answer owes nothing to how
    /// fast this machine is.
    Explore,
    /// The clock advances by what the work would have cost, so virtual time is a prediction.
    Predict(Costs),
}

impl Mode {
    fn costs(self) -> Option<Costs> {
        match self {
            Self::Explore => None,
            Self::Predict(costs) => Some(costs),
        }
    }

    fn records_latency(self) -> bool {
        matches!(self, Self::Predict(_))
    }

    /// The audit walks every account. Exploring a dozen accounts, that is affordable every step and is
    /// the whole oracle; predicting against a working set, asking every step would cost more than the
    /// run it is measuring, so it becomes a guard on the prediction being worth anything.
    fn audit_every(self) -> u64 {
        match self {
            Self::Explore => 1,
            Self::Predict(_) => 4_096,
        }
    }
}

/// How a `check` run is shaped: how long, how many accounts, and the batch policy under test. Drawn
/// from the seed by default, and named outright by a test that is about one of them.
#[derive(Debug, Clone, Copy)]
pub struct Run {
    pub steps: u64,
    pub accounts: u64,
    pub batch_size: usize,
    pub batches_in_flight: usize,
    /// Requests a client hands over at a time. A high one fills the client queue and the slot pool,
    /// which is where backpressure lives.
    pub burst: u64,
}

impl Run {
    pub fn draw(prng: &mut Prng, steps: u64, accounts: u64) -> Self {
        // Small batches and a shallow pipeline explore different orderings than large ones.
        let batch_size = 1 + (prng.next_u64() % 4) as usize;
        let batches_in_flight = 2 + (prng.next_u64() % 4) as usize;
        let burst = if prng.next_u64().is_multiple_of(3) {
            8 + prng.next_u64() % 24
        } else {
            1 + prng.next_u64() % 2
        };
        Self {
            steps,
            accounts,
            batch_size,
            batches_in_flight,
            burst,
        }
    }
}

/// How deep this tool makes its own queues for a capacity run. Every one of them is sized off the
/// client's queue depth, because a bound this tool chose must never be the answer: a request holds its
/// slot for a whole consensus round trip, so the slots have to cover that depth or the ledger refuses
/// the excess as overload — a real answer, but about this harness rather than about the hardware asked
/// about. A sizing report prints each queue's peak against its room, which is how a bound that did
/// bind gets caught rather than believed.
struct Depths {
    slots: usize,
    ack_backlog: usize,
    pending_writes: usize,
    inbox: usize,
}

impl Depths {
    fn for_queue_depth(read_queue_depth: u64) -> Self {
        let depth = read_queue_depth as usize;
        Self {
            slots: (depth * 2).next_power_of_two(),
            ack_backlog: depth,
            pending_writes: depth.max(1 << 14),
            inbox: depth.max(1 << 12),
        }
    }
}

/// What to predict: for how long, under what load, against components that answer this slowly.
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    pub duration_nanos: u64,
    /// Requests offered per virtual second, or zero to offer as fast as the ledger accepts.
    pub rate: u64,
    /// The client's queue depth: requests it has sent and not had answered, the same quantity `fio`
    /// calls `iodepth`. Saturated, this is what decides throughput — depth over latency — and it is a
    /// ceiling on what the client can ask for, not on what the ledger can do.
    pub read_queue_depth: u64,
    pub accounts: u64,
    pub costs: Costs,
    /// The pending engine as a black box: what it answers a command in. A lookup it cannot serve from
    /// memory pays the device underneath on top of this.
    pub pending_nanos: u64,
    pub pending_tail_nanos: u64,
    /// Commands a second the pending engine can answer, so it can saturate on its own.
    pub pending_rate: u64,
    pub idem_nanos: u64,
    pub raft_nanos: u64,
    /// How long a partial batch waits, from the real batch policy: it is the latency floor at low
    /// load, so a prediction that used a shorter one would propose batches the ledger would not.
    pub linger_nanos: u64,
    /// The mean of the consensus round trip's tail, on top of `raft_nanos`. A fixed round trip makes
    /// every batch equally late, which is the one thing a real quorum never is.
    pub raft_tail_nanos: u64,
    /// Account concentration, the same shape as the load driver's `--skew`.
    pub skew: f64,
    /// How old a hold is when it is resolved, in holds committed since. Zero resolves the oldest ready
    /// hold at once, which keeps every read in the newest blocks and so measures the read path's absence.
    pub resolve_after: usize,
    pub poisson: bool,
    /// Proposals consensus may have outstanding, from the real batch policy. Times the batch cap, this
    /// is how much work one round trip can hide.
    pub batches_in_flight: usize,
    /// What the same work would cost on a core nobody here has, as a percentage of this one. 100 is
    /// this machine, measured.
    pub cost_percent: u64,
    /// The engine's two memory windows in blocks, declared here because a prediction has to say which of
    /// them it assumed: what a resolution costs depends entirely on which one answers it.
    pub flush_blocks: usize,
    pub resident_blocks: usize,
    /// Retention, as the three numbers a run needs to reach it: how long a day is on the virtual clock,
    /// how many days a hold lives, and how much the sweep may offer per round. A day of zero is a run no
    /// day ever passes in, which is the default — a capacity run asks what the ledger does against
    /// components of a given speed, and a sweep competing with the traffic is a different question. It is
    /// a default rather than a fact so that question can actually be asked.
    pub day_nanos: u64,
    pub lifetime_days: u64,
    pub expiry_blocks_per_round: usize,
}

impl Plan {
    /// Commands the pending engine can have outstanding before one of them starts waiting: its
    /// rate times its latency. Under this, latency is the engine's; over it, latency is its queue's.
    pub fn pending_concurrency(&self) -> u64 {
        self.pending_rate * self.pending_nanos / 1_000_000_000
    }
}

pub struct Report {
    pub seed: u64,
    /// False when the node stopped serving before every account had money. A legitimate outcome of a
    /// fault, and a thing to report rather than wait for.
    pub funded: bool,
    pub submitted: u64,
    pub answered: u64,
    pub metrics: Metrics,
    /// The run stopped serving: a fault made the sequencer quarantine enough lanes, or seal its
    /// apply path. That is the right answer to those faults, and it also means the rest of this
    /// seed's steps explored nothing.
    pub halted: bool,
    /// Holds the engine's index could not take, counted from outside. Some seeds are given an index too
    /// small on purpose, so this is no longer required to be zero — what is required is that the ledger
    /// answered each one, which is `metrics.holds_not_stored` and the seal that follows it.
    pub overflowed: u64,
    /// Reads that went to the store. Not an invariant — zero is a correct run — but a sweep whose total
    /// is zero has not exercised the fetch path, and would say "every invariant held" about a path it
    /// never entered.
    pub store_reads: u64,
    /// Calls the store refused and blocks it answered whose checksum did not match. Both seal, so a sweep
    /// reports them for the same reason it reports `overflowed`: the seal is only tested where the path
    /// was entered.
    pub store_faults: u64,
    pub store_corruptions: u64,
    /// Pending replies that kept no place in a lane: the lookups of order-exempt resolutions. Same
    /// standing as `store_reads` — a sweep whose total is zero has covered the exemption's data check
    /// but never the exemption itself.
    pub exempt_lookups: u64,
    /// Expiry voids the engine offered because a hold outlived its retention. Same standing as the two
    /// above: not an invariant, but a sweep whose total is zero has covered no expiry at all — and expiry
    /// is what makes the index's declared maximum true rather than assumed.
    pub expiries_offered: u64,
    /// The furthest behind the sweep fell, in days. One is ordinary. More than the run's `grace_days` is
    /// the throttle behind by longer than the slack `declared_maximum` was sized with — past that a hold
    /// stays in an index that cannot grow, and late deletion has turned into a seal.
    pub days_behind_worst: u64,
}

pub struct Prediction {
    pub virtual_nanos: u64,
    pub committed: u64,
    /// Requests the client managed to submit, and the submissions they came in. A rate is offered in
    /// submissions; against it, `committed` never matches, because duplicates and refusals are part of
    /// the traffic and a chain is two requests.
    pub submitted: u64,
    /// Requests the client got an answer for. The latency below is drawn from these alone, so a run
    /// where the tail is so long that the client's queue depth never turned over reports a *better*
    /// tail from fewer samples — which is why a verdict has to look here before it looks at a quantile.
    pub answered: u64,
    /// Requests submitted and still unanswered when the measurement ended.
    pub outstanding: u64,
    pub arrivals: u64,
    pub overloaded: u64,
    /// What the core spent, and on which stage.
    pub charge: StageCharge,
    /// p50, p99, p99.9 and the worst, in microseconds of virtual time.
    pub latency_us: [f64; 4],
    /// Commands the pending engine answered, and the mean time one spent waiting for it to be free
    /// rather than being served. A wait that is a large share of its own latency means its rate, not
    /// its latency, is what the run ran into.
    pub pending_commands: u64,
    pub pending_queue_us: f64,
    /// What putting each lane back in order cost on top of the device: the mean and worst wait for an
    /// earlier read on the same lane, and the most results ever held behind one.
    pub order_wait_us: f64,
    pub order_wait_worst_us: f64,
    pub order_wait_deepest: usize,
    /// Reads the engine had to take from its store, against the lookups it answered. This is the sizing
    /// statement the two memory windows exist to make: at this resolution age and these windows, this
    /// share of resolutions costs an IO.
    pub store_reads: u64,
    /// The furthest behind the expiry sweep fell, in days. Zero in every run that passes no day, which is
    /// the default; where a day does pass, this is what says whether the throttle kept up with the traffic
    /// beside it, and more than the run's grace is where deleting late has become a seal.
    pub days_behind_worst: u64,
    /// The mean, in microseconds. Little's law is about the mean, not the median: a run held back by
    /// its queue depth answers `depth / mean`, and reading p50 there understates how long a slot is
    /// held whenever the tail is heavy.
    pub mean_us: f64,
    /// Every counter measured over the same stretch, funding excluded. Counters from the whole run
    /// divided by counters from the measurement alone was a real bug: it reported the setup's
    /// admissions as if they were the load's, so the same plan answered differently at two durations.
    pub metrics: Metrics,
    /// What the reactor and the account store were holding, and the most they ever held. Only the two
    /// components whose Rust code is real: the pending engine's store here is a stand-in's hash map,
    /// not the index the engine will have, so predicting its size would be predicting the stand-in.
    /// That question stays with the Python simulator until the structures exist.
    ///
    /// Every part carries the room it had beside the peak, so this is also where a queue this tool
    /// sized shows up as having been the limit.
    pub footprints: Vec<(&'static str, Footprint)>,
}

impl Prediction {
    /// Submissions a second — what a rate is offered in. Falls back to requests when arrivals were
    /// not counted, which is the evenly-spaced mode where a rate is enforced on requests instead.
    pub fn offered(&self) -> f64 {
        let seconds = self.virtual_nanos as f64 / 1e9;
        if seconds <= 0.0 {
            return 0.0;
        }
        let counted = if self.arrivals > 0 {
            self.arrivals
        } else {
            self.submitted
        };
        counted as f64 / seconds
    }

    pub fn throughput(&self) -> f64 {
        let seconds = self.virtual_nanos as f64 / 1e9;
        if seconds <= 0.0 {
            return 0.0;
        }
        self.committed as f64 / seconds
    }

    /// Share of what was submitted that came back. A closed loop always leaves its queue depth
    /// outstanding, so this is below one in a healthy run too — it is the size of the shortfall that
    /// says whether that depth ever turned over.
    pub fn completion(&self) -> f64 {
        self.answered as f64 / self.submitted.max(1) as f64
    }

    /// Whether the latency below is a measurement or an artefact. A client cannot have more
    /// unanswered than its queue depth, so `outstanding` is one depth's worth: answering more than that
    /// means requests came and went, while answering less means the quantiles describe a single batch
    /// still in flight — and those get *better* the slower the component is. Against `outstanding`
    /// rather than against `--qd`, because a run held back by the ledger never fills that depth at all.
    pub fn queue_depth_turned_over(&self) -> bool {
        self.answered > self.outstanding
    }

    /// How much of the virtual time the reactor's own work accounts for. Under one means it spent the
    /// rest waiting — for a component, or for load.
    pub fn core_used(&self) -> f64 {
        self.charge.total() as f64 / self.virtual_nanos.max(1) as f64
    }
}

#[derive(Debug)]
pub struct Failure {
    pub seed: u64,
    pub step: u64,
    pub broken: Broken,
    pub timings: Timings,
    pub faults: Faults,
}

type SimReactor = Reactor<MemoryAccounts, PendingFake, IdemFake, RaftFake, ManualClock>;

/// How big to build everything. The two modes want opposite things: `check` wants every queue
/// shallow enough that backpressure is reached in a few steps, and `capacity` wants them deep enough
/// that the answer is about the ledger rather than about a queue this tool chose.
struct Shape {
    accounts: u64,
    /// The batch policy under test, and how many requests a client hands over at a time.
    batch_size: usize,
    batches_in_flight: usize,
    burst: u64,
    queue: usize,
    /// Requests the client keeps unanswered.
    read_queue_depth: u64,
    /// How old a hold is when it is resolved, in holds committed since.
    resolve_after: usize,
    /// A client that only resolves holds it was told committed.
    strict: bool,
    mode: Mode,
    capacity: Capacity,
    linger_nanos: u64,
    skew: f64,
    /// Draw the gaps between arrivals instead of spacing them evenly. Nobody coordinates clients, so
    /// the even version answers a question about a load generator rather than about load.
    poisson: bool,
}

/// The counters the cost model charges against, as of the last tick.
#[derive(Default, Clone, Copy)]
struct Counts {
    admitted: u64,
    judged: u64,
    committed: u64,
}

impl Counts {
    fn of(metrics: &Metrics) -> Self {
        Self {
            admitted: metrics.admitted,
            judged: metrics.judged,
            committed: metrics.committed,
        }
    }
}

struct Sim {
    reactor: SimReactor,
    mode: Mode,
    read_queue_depth: u64,
    linger_nanos: u64,
    poisson: bool,
    prng: Prng,
    /// When the next arrival is owed, under Poisson arrivals.
    next_arrival_nanos: u64,
    slow_client_every: u64,
    steps: u64,
    requests: Producer<Request>,
    acks: Consumer<Ack>,
    clock: ManualClock,
    pending: PendingFake,
    idem: IdemFake,
    raft: RaftFake,
    traffic: Traffic,
    /// How long a day is on this clock, and the day it last told the engine about. Retention is the one
    /// thing measured in days, so a run that never crossed one would explore no expiry at all.
    day_nanos: u64,
    day: u64,
    lifetime_days: u64,
    expiry_blocks_per_round: usize,
    /// The furthest behind the expiry sweep ever fell, in days. Watched every step rather than read at
    /// the end, because it is a level: a sweep five days behind halfway through and caught up by the last
    /// step is the run that matters, and the final reading calls it zero.
    days_behind_worst: u64,
    /// Requests generated and not yet accepted by the intake queue. A submission goes in whole or
    /// waits: half a chain reaches the ledger as a chain the client never sent, because the next
    /// unlinked request is what terminates an open one.
    unsent: VecDeque<Request>,
    now: u64,
    charged: StageCharge,
    submitted: u64,
    /// Submissions, which is what a rate is about: one arrival is one thing a client does, and a chain
    /// is one arrival carrying two requests.
    arrivals: u64,
    answered: u64,
    seen: Counts,
    /// Where the offered rate is counted from. Funding submits a request per account before the
    /// measurement opens, and counting those against the rate would make the client owe nothing for
    /// the first half-second of a run.
    rate_from: (u64, u64),
    latency: Histogram<u64>,
    /// Requests the ledger refused for want of a slot. Load past what it can hold is a real answer,
    /// but it is not the answer a capacity run is after, so it is reported rather than averaged in.
    overloaded: u64,
}

/// Explores one seed for invariant breaks. Nothing about the answer depends on this machine.
pub fn check(seed: u64, steps: u64) -> Result<Report, Box<Failure>> {
    let mut prng = Prng::new(seed);
    let timings = Timings::draw(&mut prng, STEP_NANOS);
    let faults = Faults::draw(&mut prng);
    // A dozen accounts, so lanes collide and every seed exercises the same few of them hard.
    let run = Run::draw(&mut prng, steps, 12);
    explore(seed, timings, faults, run)
}

/// One seed against one set of faults. Separate from `check` so a test can name the faults it is
/// about instead of hunting for a seed that happens to draw them.
pub fn explore(
    seed: u64,
    timings: Timings,
    faults: Faults,
    run: Run,
) -> Result<Report, Box<Failure>> {
    let Run {
        steps,
        accounts,
        batch_size,
        batches_in_flight,
        burst,
    } = run;
    let mut prng = Prng::new(seed);
    let mut sim = Sim::new(
        &mut prng,
        timings,
        faults,
        Shape {
            accounts,
            batch_size,
            batches_in_flight,
            burst,
            queue: 256,
            read_queue_depth: burst * 64,
            strict: false,
            // A run of two thousand steps cannot age a hold by much, and `check`'s question is the
            // mechanism rather than the age: what puts its seeds on the store path is the narrow windows
            // `Timings::draw` gives them, not a queue of unresolved holds.
            resolve_after: 0,
            mode: Mode::Explore,
            linger_nanos: CHECK_LINGER_NANOS,
            skew: 1.0,
            poisson: false,
            capacity: Capacity {
                slots: 512,
                // Shallow enough that a client which stops reading becomes backpressure in this many
                // acks rather than in thousands.
                ack_backlog: 64,
                pending_write_backlog: 256,
                ..ReactorConfig::default().capacity
            },
        },
    );
    // A seed may fail-stop the node before funding finishes; that is one of the things being explored.
    let funded = sim.fund();
    for step in 0..steps {
        sim.offer(0);
        sim.step();
        sim.collect();
        if let Err(broken) = sim.audit_due() {
            return Err(Box::new(Failure {
                seed,
                step,
                broken,
                timings,
                faults,
            }));
        }
        // A halted sequencer refuses everything from here on, so the remaining steps would explore
        // nothing: drain and report it instead of spinning.
        if sim.reactor.is_fail_stopped() {
            break;
        }
    }
    sim.reactor.close_intake();
    let halted = sim.reactor.is_fail_stopped();
    for step in 0..steps {
        sim.step();
        sim.collect();
        if let Err(broken) = sim.reactor.audit() {
            return Err(Box::new(Failure {
                seed,
                step: steps + step,
                broken,
                timings,
                faults,
            }));
        }
    }
    let overflowed = sim.pending.overflowed();
    let store_reads = sim.pending.store_reads();
    let (store_faults, store_corruptions) = sim.pending.store_faults();
    let exempt_lookups = sim.pending.exempt_replies();
    let expiries_offered = sim.pending.expiries_offered();
    let metrics = sim.reactor.metrics();
    if metrics.invariant_breaks > 0 {
        return Err(Box::new(Failure {
            seed,
            step: steps * 2,
            broken: Broken::AccountViewDisagrees,
            timings,
            faults,
        }));
    }
    Ok(Report {
        seed,
        funded,
        submitted: sim.submitted,
        answered: sim.answered,
        metrics,
        halted,
        overflowed,
        store_reads,
        store_faults,
        store_corruptions,
        exempt_lookups,
        expiries_offered,
        days_behind_worst: sim.days_behind_worst,
    })
}

/// Predicts what a run costs against components that answer as slowly as the plan says. The audit
/// runs here too: a prediction from a ledger that broke an invariant is worth nothing.
pub fn capacity(plan: Plan) -> Result<Prediction, Box<Failure>> {
    let mut prng = Prng::new(0x5ca1_ab1e);
    let timings = Timings::from(&plan);
    let depths = Depths::for_queue_depth(plan.read_queue_depth);
    let faults = Faults::none(depths.inbox);
    let mut sim = Sim::new(
        &mut prng,
        timings,
        faults,
        Shape {
            accounts: plan.accounts,
            batch_size: 1_000,
            batches_in_flight: plan.batches_in_flight,
            burst: 32,
            queue: 1 << 12,
            read_queue_depth: plan.read_queue_depth,
            strict: true,
            mode: Mode::Predict(plan.costs.scaled(plan.cost_percent)),
            linger_nanos: plan.linger_nanos,
            skew: plan.skew,
            resolve_after: plan.resolve_after,
            poisson: plan.poisson,
            capacity: Capacity {
                slots: depths.slots,
                ack_backlog: depths.ack_backlog,
                pending_write_backlog: depths.pending_writes,
                ..ReactorConfig::default().capacity
            },
        },
    );
    assert!(
        sim.fund(),
        "the ledger stopped serving while funding, with no fault injected: that is a defect, not a plan"
    );
    let opened = sim.now;
    sim.rate_from = (sim.now, sim.submitted);
    sim.next_arrival_nanos = sim.now;
    // The measurement opens here. Funding is a fixed cost that does not grow with the duration, so
    // anything it did is taken out rather than divided into what the load did.
    let before = sim.reactor.metrics();
    let submitted_before = sim.submitted;
    let answered_before = sim.answered;
    let arrivals_before = sim.arrivals;
    let overloaded_before = sim.overloaded;
    let charged_before = sim.charged;
    sim.pending.reset_stats();
    sim.latency.reset();
    // The audit walks every account, and a capacity run has a working set rather than a dozen
    // accounts: asking every step would cost more than the run it is measuring. Correctness under
    // interleaving is `check`'s question; here the audit is a guard on the prediction being worth
    // anything at all.
    while sim.now - opened < plan.duration_nanos {
        sim.offer(plan.rate);
        sim.step();
        sim.collect();
        if let Err(broken) = sim.audit_due() {
            return Err(Box::new(Failure {
                seed: 0,
                step: sim.now,
                broken,
                timings,
                faults,
            }));
        }
    }
    if let Err(broken) = sim.reactor.audit() {
        return Err(Box::new(Failure {
            seed: 0,
            step: sim.now,
            broken,
            timings,
            faults,
        }));
    }
    let engine = sim.pending.engine();
    let order_wait = sim.pending.order_wait();
    let metrics = sim.reactor.metrics().since(&before);
    let percentile = |q: f64| sim.latency.value_at_quantile(q) as f64 / 1_000.0;
    Ok(Prediction {
        virtual_nanos: sim.now - opened,
        committed: metrics.committed,
        submitted: sim.submitted - submitted_before,
        answered: sim.answered - answered_before,
        outstanding: sim.submitted.saturating_sub(sim.answered),
        arrivals: sim.arrivals - arrivals_before,
        overloaded: sim.overloaded - overloaded_before,
        charge: sim.charged.since(&charged_before),
        latency_us: [
            percentile(0.5),
            percentile(0.99),
            percentile(0.999),
            percentile(1.0),
        ],
        mean_us: sim.latency.mean() / 1_000.0,
        days_behind_worst: sim.days_behind_worst,
        pending_commands: engine.reads,
        pending_queue_us: engine.queued_nanos as f64 / engine.reads.max(1) as f64 / 1_000.0,
        order_wait_us: order_wait.waited_nanos as f64 / order_wait.released.max(1) as f64 / 1_000.0,
        order_wait_worst_us: order_wait.worst_nanos as f64 / 1_000.0,
        order_wait_deepest: order_wait.deepest,
        store_reads: sim.pending.store_reads(),
        metrics,
        footprints: vec![
            ("sequencer", sim.reactor.footprint()),
            ("accounts", sim.reactor.accounts().footprint()),
        ],
    })
}

impl Sim {
    fn new(prng: &mut Prng, timings: Timings, faults: Faults, shape: Shape) -> Self {
        let Shape {
            accounts,
            batch_size,
            batches_in_flight,
            burst,
            queue,
            read_queue_depth,
            strict,
            mode,
            capacity,
            linger_nanos,
            skew,
            resolve_after,
            poisson,
        } = shape;
        let mut store = MemoryAccounts::with_capacity(accounts as usize + 1);
        store.open(EXTERNAL, LEDGER, AccountFlags::NONE);
        for index in 0..accounts {
            store.open(
                AccountId(FIRST_USER + index),
                LEDGER,
                AccountFlags::CONSTRAINED,
            );
        }
        let clock = ManualClock::new(0);
        let (requests, request_rx) = channel(queue);
        let (ack_tx, acks) = channel(queue);
        let pending = PendingFake::new(timings, faults, prng.next_u64());
        let idem = IdemFake::new(timings, faults);
        let raft = RaftFake::new(timings, faults, prng.next_u64());
        let config = ReactorConfig {
            capacity,
            batching: BatchPolicy {
                size: batch_size,
                linger: std::time::Duration::from_nanos(linger_nanos),
                in_flight: batches_in_flight,
                ..ReactorConfig::default().batching
            },
            ..ReactorConfig::default()
        };
        let (reactor, _log) = Reactor::with_clock(
            config,
            Transport {
                requests: request_rx,
                acks: ack_tx,
            },
            store,
            pending.clone(),
            idem.clone(),
            raft.clone(),
            clock.clone(),
        )
        .expect("the simulation's config is valid");
        Self {
            reactor,
            mode,
            read_queue_depth,
            linger_nanos,
            poisson,
            prng: Prng::new(prng.next_u64()),
            next_arrival_nanos: 0,
            slow_client_every: faults.slow_client_every,
            steps: 0,
            requests,
            acks,
            clock,
            pending,
            idem,
            raft,
            // Everything the client has outstanding can be a hold, so the list is sized by the
            // client's queue depth rather than by a constant.
            traffic: Traffic::new(
                prng.next_u64(),
                burst,
                accounts,
                strict,
                read_queue_depth as usize,
                skew,
                resolve_after,
            ),
            day_nanos: timings.day_nanos,
            day: 0,
            lifetime_days: timings.lifetime_days,
            expiry_blocks_per_round: timings.expiry_blocks_per_round,
            days_behind_worst: 0,
            unsent: VecDeque::new(),
            now: 0,
            charged: StageCharge::default(),
            submitted: 0,
            arrivals: 0,
            answered: 0,
            seen: Counts::default(),
            rate_from: (0, 0),
            latency: Histogram::new_with_bounds(1, 60_000_000_000, 3).expect("histogram bounds"),
            overloaded: 0,
        }
    }

    /// Every account starts with money, through the ledger's own path — and is checked to have got
    /// it. A refused funding transfer is retried, because an account left at zero would spend the
    /// rest of the run being refused for insufficient balance, and a run like that measures nothing.
    /// False when the sequencer stopped serving before every account was funded, which a seed's faults
    /// are allowed to cause: the run then explores a halted node, and saying so beats waiting forever
    /// for a commit that is never coming.
    fn fund(&mut self) -> bool {
        let mut owed: Vec<AccountId> = (0..self.traffic.accounts())
            .map(|index| AccountId(FIRST_USER + index))
            .collect();
        let mut sent: FxHashMap<TxId, AccountId> = FxHashMap::default();
        let mut idle = 0;
        while !owed.is_empty() || !sent.is_empty() {
            if self.reactor.is_fail_stopped() {
                return false;
            }
            // In batches the ledger can hold: the whole set at once would be refused as overload,
            // retried, and refused again.
            while sent.len() < self.read_queue_depth as usize {
                let Some(account) = owed.pop() else { break };
                let transfer = self.traffic.funding(account, FUNDING);
                if self
                    .requests
                    .push(Request::single(transfer, self.now))
                    .is_err()
                {
                    owed.push(account);
                    break;
                }
                sent.insert(transfer.id, account);
                self.submitted += 1;
            }
            let before = sent.len() + owed.len();
            self.step();
            while let Some(ack) = self.acks.pop() {
                self.answered += 1;
                if let Some(account) = sent.remove(&ack.tx_id) {
                    if !matches!(ack.outcome, AckOutcome::Committed) {
                        owed.push(account);
                    }
                }
            }
            idle = if sent.len() + owed.len() < before {
                0
            } else {
                idle + 1
            };
            if idle > 10_000_000 {
                return false;
            }
        }
        true
    }

    /// The ledger's own audit, as often as the mode calls for.
    fn audit_due(&self) -> Result<(), Broken> {
        if self.steps.is_multiple_of(self.mode.audit_every()) {
            return self.reactor.audit();
        }
        Ok(())
    }

    /// One instant for the reactor and for the components: they read the same virtual clock. In check
    /// mode the clock moves a fixed step; in capacity mode it moves by what the tick's work would
    /// have cost, which is what turns virtual time into a prediction.
    fn step(&mut self) {
        self.steps += 1;
        let progress = self.reactor.tick();
        let advance = match self.mode.costs() {
            None => STEP_NANOS,
            Some(costs) => {
                let charged = self.charge(costs, progress);
                // A tick that did nothing is waiting for a component, so jump to whenever that
                // component next has something to say instead of crawling there in idle steps. The
                // jump stops at the batch linger, because a partial batch is due before that.
                if charged > 0 {
                    charged
                } else {
                    let ceiling = self.now + self.linger_nanos;
                    let next = self
                        .next_due()
                        .unwrap_or(ceiling)
                        .clamp(self.now + 1, ceiling);
                    next - self.now
                }
            }
        };
        self.now += advance;
        self.clock.advance(advance);
        self.advance_day();
        self.pending.drive(self.now);
        self.idem.drive(self.now);
        self.raft.drive(self.now);
    }

    /// Tells the engine what day it is, and lets it offer the next slice of whatever ran out. The day is
    /// handed in rather than read from a clock, which is what lets a two-thousand-step run cross a
    /// retention window that is measured in days.
    fn advance_day(&mut self) {
        if self.day_nanos == 0 {
            return;
        }
        let day = self.now / self.day_nanos;
        if day != self.day {
            self.day = day;
            self.pending.open_day(day, self.lifetime_days);
        }
        self.pending.sweep_expiry(self.expiry_blocks_per_round);
        self.days_behind_worst = self.days_behind_worst.max(self.pending.days_behind());
    }

    /// The earliest any component has something to hand back.
    fn next_due(&self) -> Option<u64> {
        [
            self.pending.next_due(),
            self.idem.next_due(),
            self.raft.next_due(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// What the core spent this tick, charged to the stage that spent it. A tick that got somewhere
    /// without finishing a stage is floored at `tick_ns` rather than costing nothing, and the floor is
    /// charged as well as advanced: time the clock passes and nothing accounts for is reported as
    /// waiting on a component.
    fn charge(&mut self, costs: Costs, progress: bool) -> u64 {
        let now = Counts::of(&self.reactor.metrics());
        let judged = (now.judged - self.seen.judged) as f64;
        let stages = StageCharge {
            intake_ns: ((now.admitted - self.seen.admitted) as f64 * costs.intake_ns) as u64,
            judge_ns: (judged * costs.judge_ns) as u64,
            propose_ns: (judged * costs.propose_ns) as u64,
            apply_ns: ((now.committed - self.seen.committed) as f64 * costs.apply_ns) as u64,
            tick_ns: 0,
        };
        self.seen = now;
        let work = stages.total();
        // A tick that moved something but finished no stage still ran, and the floor is its whole
        // cost — charging it on top of a stage would price the same tick twice.
        let floor = if progress && work == 0 {
            costs.tick_ns as u64
        } else {
            0
        };
        self.charged.intake_ns += stages.intake_ns;
        self.charged.judge_ns += stages.judge_ns;
        self.charged.propose_ns += stages.propose_ns;
        self.charged.apply_ns += stages.apply_ns;
        self.charged.tick_ns += floor;
        work + floor
    }

    /// A client that stops reading now and then, which is what fills the ack backlog.
    fn reading(&self) -> bool {
        self.slow_client_every == 0 || !self.steps.is_multiple_of(self.slow_client_every)
    }

    fn collect(&mut self) {
        if !self.reading() {
            return;
        }
        while let Some(ack) = self.acks.pop() {
            self.answered += 1;
            // Only what committed: a rejection is answered at intake, so counting those would put a
            // crowd of near-zero samples in front of every quantile.
            if self.mode.records_latency() && matches!(ack.outcome, AckOutcome::Committed) {
                let waited = self.now.saturating_sub(ack.submitted_at_nanos).max(1);
                self.latency.saturating_record(waited);
            }
            if matches!(ack.outcome, AckOutcome::Rejected(LedgerError::Overloaded)) {
                self.overloaded += 1;
            }
            self.traffic
                .answered(&ack, matches!(ack.outcome, AckOutcome::Committed));
        }
    }

    /// Hands over everything already generated, stopping at the first refusal. A submission is never
    /// left half-sent: the ledger terminates an open chain with the next unlinked request, so a
    /// dropped second leg would join a stranger's chain and be judged with it.
    fn flush(&mut self) -> bool {
        while let Some(&request) = self.unsent.front() {
            if self.requests.push(request).is_err() {
                return false;
            }
            self.unsent.pop_front();
            self.submitted += 1;
        }
        true
    }

    /// Offers load. At a rate, the clock decides how much is owed; saturated, the queue depth does —
    /// keep the ledger as busy as a client with that many requests outstanding would, since a step's
    /// virtual length varies and load offered per step would not be a rate at all.
    ///
    /// Bounded, because the traffic generator is allowed to offer nothing on a turn: without a bound
    /// a saturated run would spin here forever waiting for it to offer something.
    fn offer(&mut self, rate: u64) {
        let depth = self.read_queue_depth;
        // Bounded, but generously: a step can cover many arrival gaps when the clock jumps to the next
        // component event, and those arrivals are owed rather than lost.
        for _ in 0..256 {
            // Nothing new is generated while a submission is still waiting for room, which is what
            // keeps the backlog to one submission.
            if !self.flush() {
                return;
            }
            if self.submitted.saturating_sub(self.answered) >= depth {
                return;
            }
            if rate > 0 {
                if self.poisson {
                    // One arrival at a time, its gap drawn: the burstiness is the point, since a tail
                    // is made of the moments when several arrive at once. The next arrival is counted
                    // from the last one, not from now, or a step that covered ten gaps would deliver
                    // one arrival and silently drop nine.
                    if self.now < self.next_arrival_nanos {
                        return;
                    }
                    let mean = 1_000_000_000 / rate.max(1);
                    self.next_arrival_nanos += self.prng.exponential_nanos(mean).max(1);
                    self.arrivals += 1;
                    self.unsent.extend(self.traffic.single(self.now));
                    continue;
                }
                let (since, already) = self.rate_from;
                let owed = already
                    + (u128::from(self.now - since) * u128::from(rate) / 1_000_000_000) as u64;
                if self.submitted >= owed {
                    return;
                }
            }
            self.unsent.extend(self.traffic.next(self.now));
        }
    }
}
