use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::sync::Mutex;

use ledger_base::ports::{ApplyIndex, RaftCommit, RaftOutcome, RaftPort, RaftProposal};
use ledger_base::Effect;
use ledger_base::{channel, Consumer, Footprint, MapGauge, Prng, Producer, StagedProducer};
use ledger_stubkit::{AnswerGate, IdleBackoff, LatencyRange, WorkerThread};

#[derive(Debug, Clone, Copy)]
pub struct EchoRaftConfig {
    pub queue_capacity: usize,
    pub round_trip: LatencyRange,
    pub fail_every: u64,
    /// Answer every nth pair of proposals in the wrong order, so the sequencer's own check on
    /// commit order can be exercised. Zero keeps consensus well behaved.
    pub reorder_every: u64,
    pub seed: u64,
    /// Keeps a copy of every committed effect so a test can replay the log. Off by default:
    /// a real log lives on disk, not in a vector.
    pub keep_log: bool,
}

impl Default for EchoRaftConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            round_trip: LatencyRange::new(Duration::from_micros(900), Duration::from_micros(1400)),
            fail_every: 0,
            reorder_every: 0,
            seed: 0x8aff_9e37,
            keep_log: false,
        }
    }
}

/// One entry the log keeps. It is also what a *durable* log writes per effect, which is the figure a
/// sizing model needs and the one nothing here bounds: there is no compaction, so the count is the
/// run's whole volume until a snapshot cuts it.
pub const LOG_EFFECT_BYTES: usize = size_of::<Effect>();
/// One proposal outstanding, apart from the effects it carries.
pub const PROPOSAL_BYTES: usize = size_of::<RaftProposal>();

/// Stand-in for consensus: batches commit in proposal order after a round trip, and the
/// sequencer never waits for one. Real replication across five nodes is not built yet.
pub struct EchoRaft {
    proposals: Producer<RaftProposal>,
    commits: Consumer<RaftCommit>,
    log: Arc<Mutex<Vec<Effect>>>,
    /// Proposals the worker is holding, and the effects inside them. Published by the worker because
    /// they live on its thread, and they are real memory: a batch waits here for its whole round trip.
    inflight: Arc<InflightGauge>,
    /// A test's hold on the commits — see `AnswerGate`. Open unless somebody closed it.
    commits_gate: AnswerGate,
    _thread: WorkerThread,
}

#[derive(Debug, Default)]
struct InflightGauge {
    proposals: MapGauge,
    effects: MapGauge,
}

impl EchoRaft {
    pub fn start(config: EchoRaftConfig) -> Self {
        let (proposals, proposal_rx) = channel(config.queue_capacity);
        let (commit_tx, commits) = channel(config.queue_capacity);
        let log = Arc::new(Mutex::new(Vec::new()));
        let worker_log = Arc::clone(&log);
        let inflight = Arc::new(InflightGauge::default());
        let worker_inflight = Arc::clone(&inflight);
        let commits_gate = AnswerGate::default();
        let worker_gate = commits_gate.clone();
        let thread = WorkerThread::spawn("raft", move |shutdown| {
            RaftWorker {
                log: worker_log,
                keep_log: config.keep_log,
                proposals: proposal_rx,
                commits: StagedProducer::new(commit_tx),
                inflight: VecDeque::new(),
                gauge: worker_inflight,
                jitter: Prng::new(config.seed),
                round_trip: config.round_trip,
                fail_every: config.fail_every,
                reorder_every: config.reorder_every,
                proposals_seen: 0,
                next_index: 0,
                gate: worker_gate,
            }
            .run(shutdown)
        });
        Self {
            proposals,
            commits,
            log,
            inflight,
            commits_gate,
            _thread: thread,
        }
    }

    /// A hold on the commits, for a test that has to see a batch still in flight — see `AnswerGate`.
    /// What it replaces is a round trip long enough that the test hoped consensus had not answered yet.
    pub fn commits(&self) -> AnswerGate {
        self.commits_gate.clone()
    }

    /// What consensus is holding. The log is only kept when a run asked for it, so an empty log here is
    /// not a claim that a real log would be empty — it is this stand-in not keeping one. There is no
    /// snapshot or compaction either, which is why a sizing report reads the log's growth as a volume
    /// rather than a steady size.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        let (entries, capacity) = self
            .log
            .lock()
            .map(|log| (log.len(), log.capacity()))
            .unwrap_or_default();
        footprint.other("kept log", entries, entries, 0, capacity * LOG_EFFECT_BYTES);
        let effects = self.inflight.effects.peak();
        footprint.other(
            "proposals in flight",
            self.inflight.proposals.entries(),
            self.inflight.proposals.peak(),
            0,
            self.inflight.proposals.capacity() * PROPOSAL_BYTES + effects * LOG_EFFECT_BYTES,
        );
        footprint
    }

    /// The committed log in order, for replay checks.
    pub fn log(&self) -> Vec<Effect> {
        self.log.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

impl RaftPort for EchoRaft {
    fn propose(&self, proposal: RaftProposal) -> Result<(), RaftProposal> {
        self.proposals.push(proposal)
    }

    fn poll(&self) -> Option<RaftCommit> {
        self.commits.pop()
    }
}

struct RaftWorker {
    log: Arc<Mutex<Vec<Effect>>>,
    gauge: Arc<InflightGauge>,
    keep_log: bool,
    proposals: Consumer<RaftProposal>,
    commits: StagedProducer<RaftCommit>,
    inflight: VecDeque<(Instant, RaftProposal, RaftOutcome)>,
    jitter: Prng,
    round_trip: LatencyRange,
    fail_every: u64,
    reorder_every: u64,
    proposals_seen: u64,
    gate: AnswerGate,
    /// The log position the next committed batch takes. A real log would have this durably; here it is a
    /// counter, which is enough to give the sequencer a position to record and is the point of it existing
    /// before consensus does — see `ApplyIndex`.
    next_index: u64,
}

impl RaftWorker {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut backoff = IdleBackoff::new();
        while !shutdown.load(Ordering::Relaxed) {
            let progress = self.drain_proposals() | self.deliver();
            backoff.record(progress);
        }
    }

    /// Once per round rather than once per proposal: a report asks at the end of a run.
    fn publish(&self) {
        self.gauge
            .proposals
            .publish(self.inflight.len(), self.inflight.capacity());
        let effects: usize = self
            .inflight
            .iter()
            .map(|(_, p, _)| p.effects.capacity())
            .sum();
        self.gauge.effects.publish(effects, effects);
    }

    fn drain_proposals(&mut self) -> bool {
        let mut progress = false;
        while let Some(proposal) = self.proposals.pop() {
            progress = true;
            self.proposals_seen += 1;
            let outcome =
                if self.fail_every > 0 && self.proposals_seen.is_multiple_of(self.fail_every) {
                    RaftOutcome::Failed
                } else {
                    RaftOutcome::Committed
                };
            let due = self.round_trip.due_from(Instant::now(), &mut self.jitter);
            self.inflight.push_back((due, proposal, outcome));
            if self.reorder_every > 0
                && self.proposals_seen.is_multiple_of(self.reorder_every)
                && self.inflight.len() >= 2
            {
                let last = self.inflight.len() - 1;
                self.inflight.swap(last - 1, last);
            }
        }
        if progress {
            self.publish();
        }
        progress
    }

    fn deliver(&mut self) -> bool {
        // A test holding the commits back. The worker keeps taking proposals and timing them; only what
        // would leave is kept, which is what makes "still in flight" a state a test can wait on rather
        // than a round trip it has to hope was long enough.
        if !self.gate.is_open() {
            self.gate.note_waiting(self.inflight.len());
        }
        if !self.commits.flush() {
            return false;
        }
        let now = Instant::now();
        let mut progress = false;
        while !self.commits.is_stuck() && self.gate.may_send() {
            let ready = self.inflight.front().is_some_and(|(due, _, _)| *due <= now);
            if !ready {
                break;
            }
            if let Some((_, proposal, outcome)) = self.inflight.pop_front() {
                if self.keep_log && outcome == RaftOutcome::Committed {
                    if let Ok(mut log) = self.log.lock() {
                        log.extend_from_slice(&proposal.effects);
                    }
                }
                // A committed batch takes the next position; a refused one takes none, because nothing
                // was written. So the index counts what is in the log rather than what was proposed, which
                // is what makes it gapless — the property recovery rests on.
                if outcome == RaftOutcome::Committed {
                    self.next_index += 1;
                }
                self.gate.spend();
                self.commits.send(RaftCommit {
                    batch_id: proposal.batch_id,
                    index: ApplyIndex(self.next_index),
                    outcome,
                    effects: proposal.effects,
                });
                progress = true;
            }
        }
        progress
    }
}
