use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ledger_base::ports::{IdemReply, IdemRequest, IdemVerdict, IdempotencyPort};
use ledger_base::{
    Consumer, Footprint, FxHashMap, MapGauge, Prng, Producer, StagedProducer, TxId, channel,
};
use ledger_stubkit::{IdleBackoff, LaneOrderer, LatencyRange, WorkerThread};

#[derive(Debug, Clone, Copy)]
pub struct MemoryDedupConfig {
    pub queue_capacity: usize,
    pub latency: LatencyRange,
    pub violate_order_every: u32,
    pub seed: u64,
}

impl Default for MemoryDedupConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 8192,
            latency: LatencyRange::new(Duration::from_micros(1), Duration::from_micros(5)),
            violate_order_every: 0,
            seed: 0x1de3_9e37,
        }
    }
}

/// In-memory dedup, run off the reactor core because each verdict is independent of every
/// other. The rotating generations that expire the one-hour window are not built yet, so
/// this map only grows.
pub struct MemoryDedup {
    requests: Producer<IdemRequest>,
    results: Consumer<IdemReply>,
    /// What the map is holding, published by the worker because the map lives on its thread.
    seen: Arc<MapGauge>,
    _thread: WorkerThread,
}

impl MemoryDedup {
    pub fn start(config: MemoryDedupConfig) -> Self {
        let (requests, request_rx) = channel(config.queue_capacity);
        let (result_tx, results) = channel(config.queue_capacity);
        let seen = Arc::new(MapGauge::default());
        let worker_seen = Arc::clone(&seen);
        let thread = WorkerThread::spawn("idempotency", move |shutdown| {
            IdemWorker {
                requests: request_rx,
                results: StagedProducer::new(result_tx),
                seen: FxHashMap::default(),
                gauge: worker_seen,
                orderer: LaneOrderer::new(config.violate_order_every),
                jitter: Prng::new(config.seed),
                latency: config.latency,
            }
            .run(shutdown)
        });
        Self {
            requests,
            results,
            seen,
            _thread: thread,
        }
    }

    /// What this component is holding. The rotating generations that would expire the one-hour window
    /// are not built, so the map only grows: this figure is the whole run's transactions, not a
    /// steady state, and sizing the real thing needs the expiry that does not exist yet.
    pub fn footprint(&self) -> Footprint {
        let mut footprint = Footprint::new();
        footprint.gauged_table::<TxId, u64>("dedup keys", &self.seen);
        footprint
    }
}

impl IdempotencyPort for MemoryDedup {
    fn dispatch(&self, request: IdemRequest) -> Result<(), IdemRequest> {
        self.requests.push(request)
    }

    fn poll(&self) -> Option<IdemReply> {
        self.results.pop()
    }
}

struct IdemWorker {
    requests: Consumer<IdemRequest>,
    results: StagedProducer<IdemReply>,
    seen: FxHashMap<TxId, u64>,
    gauge: Arc<MapGauge>,
    orderer: LaneOrderer<IdemReply>,
    jitter: Prng,
    latency: LatencyRange,
}

impl IdemWorker {
    fn run(mut self, shutdown: Arc<AtomicBool>) {
        let mut backoff = IdleBackoff::new();
        while !shutdown.load(Ordering::Relaxed) {
            let progress = self.drain_requests() | self.deliver();
            backoff.record(progress);
        }
    }

    fn drain_requests(&mut self) -> bool {
        let mut progress = false;
        while let Some(request) = self.requests.pop() {
            progress = true;
            let verdict = match self.seen.insert(request.tx_id, request.digest) {
                None => IdemVerdict::Fresh,
                Some(digest) if digest == request.digest => IdemVerdict::DuplicateSameBody,
                Some(_) => IdemVerdict::DuplicateDifferentBody,
            };
            let due = self.latency.due_from(Instant::now(), &mut self.jitter);
            self.orderer.push(
                request.lane,
                due,
                IdemReply {
                    correlation: request.correlation,
                    lane: request.lane,
                    seq: request.seq,
                    verdict,
                },
            );
        }
        // Once per round rather than once per request: a report asks at the end of a run, and paying
        // for it per request would be a cost on the path this component exists to keep cheap.
        if progress {
            self.gauge.publish(self.seen.len(), self.seen.capacity());
        }
        progress
    }

    fn deliver(&mut self) -> bool {
        if !self.results.flush() {
            return false;
        }
        let now = Instant::now();
        let mut progress = false;
        while !self.results.is_stuck() {
            match self.orderer.pop_ready(now) {
                Some(result) => {
                    self.results.send(result);
                    progress = true;
                }
                None => break,
            }
        }
        progress
    }
}
