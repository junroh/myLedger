use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::sync::Mutex;

use ledger_base::ports::{RaftCommit, RaftOutcome, RaftPort, RaftProposal};
use ledger_base::Effect;
use ledger_base::{Consumer, Footprint, MapGauge, Prng, Producer, StagedProducer, channel};
use ledger_stubkit::{IdleBackoff, LatencyRange, WorkerThread};

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

/// Stand-in for consensus: batches commit in proposal order after a round trip, and the
/// sequencer never waits for one. Real replication across five nodes is not built yet.
pub struct EchoRaft {
    proposals: Producer<RaftProposal>,
    commits: Consumer<RaftCommit>,
    log: Arc<Mutex<Vec<Effect>>>,
    /// Proposals the worker is holding, and the effects inside them. Published by the worker because
    /// they live on its thread, and they are real memory: a batch waits here for its whole round trip.
    inflight: Arc<InflightGauge>,
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
            }
            .run(shutdown)
        });
        Self { proposals, commits, log, inflight, _thread: thread }
    }

    /// What consensus is holding. The log is only kept when a run asked for it, so an empty log here is
    /// not a claim that a real log would be empty — it is this stand-in not keeping one. There is no
    /// snapshot or compaction either, which is why a sizing report reads the log's growth as a volume
    /// rather than a steady size.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        let (entries, capacity) =
            self.log.lock().map(|log| (log.len(), log.capacity())).unwrap_or_default();
        footprint.other("kept log", entries, entries, 0, capacity * size_of::<Effect>());
        let effects = self.inflight.effects.peak();
        footprint.other(
            "proposals in flight",
            self.inflight.proposals.entries(),
            self.inflight.proposals.peak(),
            0,
            self.inflight.proposals.capacity() * size_of::<RaftProposal>()
                + effects * size_of::<Effect>(),
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
        self.gauge.proposals.publish(self.inflight.len(), self.inflight.capacity());
        let effects: usize = self.inflight.iter().map(|(_, p, _)| p.effects.capacity()).sum();
        self.gauge.effects.publish(effects, effects);
    }

    fn drain_proposals(&mut self) -> bool {
        let mut progress = false;
        while let Some(proposal) = self.proposals.pop() {
            progress = true;
            self.proposals_seen += 1;
            let outcome = if self.fail_every > 0 && self.proposals_seen.is_multiple_of(self.fail_every) {
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
        if !self.commits.flush() {
            return false;
        }
        let now = Instant::now();
        let mut progress = false;
        while !self.commits.is_stuck() {
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
                self.commits.send(RaftCommit {
                    batch_id: proposal.batch_id,
                    outcome,
                    effects: proposal.effects,
                });
                progress = true;
            }
        }
        progress
    }
}
