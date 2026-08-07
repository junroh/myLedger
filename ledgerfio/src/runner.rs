use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort};
use ledger_base::{Ack, AckOutcome, LedgerError, Transfer};
use ledger_idempotency::{MemoryIdem, MemoryIdemConfig};
use ledger_pending::{
    DaySource, MemoryPending, MemoryPendingConfig, OpenBacking, PendingStorage, SnapshotDir,
    SnapshotPolicy, StoreModel,
};
use ledger_raft::{EchoRaft, EchoRaftConfig};
use ledger_sequencer::{BatchPolicy, ReactorConfig};
use ledger_service::{ClientEndpoint, LedgerService, ServiceConfig};

use crate::cli::Options;
use crate::client::Client;
use crate::histogram::Histogram;
use crate::report::{LatencySummary, RunReport};
use crate::workload::{Shape, Workload, CLEARING_ACCOUNT, EXTERNAL_ACCOUNT};
use ledger_base::Signals;

const LEDGER: u32 = 1;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Runner {
    options: Options,
}

impl Runner {
    pub fn new(options: Options) -> Self {
        Self { options }
    }

    pub fn run(self) -> RunReport {
        let options = self.options;
        let shape = Shape {
            accounts: options.accounts,
            skew: options.skew,
            external_ratio: options.external_ratio,
            resolve_after: options.resolve_after,
        };
        let mut workload = Workload::new(options.workload, shape, options.seed);
        let (service, endpoint, calendar) = Self::start_ledger(&options, &workload);
        let reactor_started = Instant::now();

        // A signal now stops the run: the driver stops submitting and the service drains.
        Signals::install();
        let stop = service.stop_token();
        let mut driver = Driver::new(
            Client::new(endpoint.requests, endpoint.acks),
            options.client_batch,
        );
        driver.fund(&mut workload);
        let elapsed = driver.measure(&mut workload, &options, &calendar);
        if Signals::requested() {
            eprintln!("ledgerfio: interrupted, draining");
        }
        stop.request();
        let stopped = service.shutdown().expect("service stopped");
        let reactor_elapsed = reactor_started.elapsed();
        if !stopped.drained {
            eprintln!("ledgerfio: shutdown abandoned work still in flight");
        }

        RunReport {
            workload: options.workload.name(),
            accounts: workload.account_count(),
            elapsed,
            reactor_elapsed,
            submitted: driver.submitted,
            committed: driver.committed,
            duplicates: driver.duplicates,
            rejected: driver.rejected,
            reject_kinds: driver.reject_kinds,
            latency: LatencySummary::from(&driver.histogram),
            batch_latency: driver.batches.summary(),
            metrics: stopped.reactor.metrics(),
            stages: stopped.reactor.stage_times(),
            profiled: options.profile,
            slo_p999: options.slo_p999,
            totals: stopped.reactor.accounts().totals(),
            overlay: stopped.reactor.overlay_total(),
            quarantined: stopped.reactor.quarantined().len(),
            fail_stop: stopped.reactor.is_fail_stopped(),
            placement: stopped.placement,
            // In the order a request meets them, so the block reads as a path rather than a list.
            footprints: vec![
                ("sequencer", stopped.reactor.footprint()),
                ("accounts", stopped.reactor.accounts().footprint()),
                ("idem", stopped.reactor.idem().footprint()),
                ("pending engine", stopped.reactor.pending().footprint()),
                ("consensus", stopped.reactor.raft().footprint()),
            ],
            pending_traffic: stopped.reactor.pending().traffic(),
            order_wait: stopped.reactor.pending().order_wait(),
            drain_work: stopped.reactor.pending().drain_work(),
            snapshots: stopped.reactor.pending().snapshots(),
            snapshot_shadow_budget: options.snapshot_shadow,
        }
    }

    /// The whole node: the reactor and the three components it talks to, each simulated with the
    /// latency and the faults this run asked for.
    ///
    /// The calendar comes back with it, because a run that asked to cross a day has to be the one moving
    /// it — the engine reads the day and never sets it.
    fn start_ledger(
        options: &Options,
        workload: &Workload,
    ) -> (
        LedgerService<MemoryAccounts, MemoryPending, MemoryIdem, EchoRaft>,
        ClientEndpoint,
        Calendar,
    ) {
        let calendar = Calendar::new(options.expiry_days);
        let pending = MemoryPending::start_with_days(
            MemoryPendingConfig {
                violate_order_every: options.violate_order_every,
                seed: options.seed ^ 0x9e37,
                overlay_soft_limit: options.overlay_limit,
                store: StoreModel {
                    read_base_nanos: options.store_read.min.as_nanos() as u64,
                    read_tail_nanos: (options
                        .store_read
                        .max
                        .saturating_sub(options.store_read.min))
                    .as_nanos() as u64,
                    write_base_nanos: options.store_write.min.as_nanos() as u64,
                    write_tail_nanos: (options
                        .store_write
                        .max
                        .saturating_sub(options.store_write.min))
                    .as_nanos() as u64,
                    sync_base_nanos: options.store_sync.min.as_nanos() as u64,
                    sync_tail_nanos: (options
                        .store_sync
                        .max
                        .saturating_sub(options.store_sync.min))
                    .as_nanos() as u64,
                    iops: options.store_iops,
                    queue_depth: options.store_queue_depth,
                    fault_every: options.store_fault_every,
                    corrupt_every: options.store_corrupt_every,
                },
                capacity: options.capacity,
                index_budget_bytes: options.index_budget as usize,
                expiry_blocks_per_round: options.expiry_blocks_per_round,
                snapshot: SnapshotPolicy {
                    // A cadence with nowhere to write is a policy that does nothing, so the directory is
                    // what turns it on — the same rule `--store-dir` follows.
                    every: match options.snapshot_dir {
                        None => 0,
                        Some(_) => options.snapshot_every,
                    },
                    bytes_per_round: options.snapshot_bytes,
                    shadow_budget: options.snapshot_shadow,
                },
                ..MemoryPendingConfig::default()
            },
            calendar.source(),
            PendingStorage {
                blocks: match options.store_dir {
                    None => OpenBacking::Memory,
                    Some(dir) => OpenBacking::files(
                        std::path::Path::new(dir),
                        options.store_read_threads,
                        options.store_write_lane,
                    )
                    .unwrap_or_else(|_| {
                        eprintln!("ledgerfio: --store-dir {dir} cannot be opened");
                        std::process::exit(2);
                    }),
                },
                snapshots: options.snapshot_dir.map(|dir| {
                    SnapshotDir::open(std::path::Path::new(dir)).unwrap_or_else(|err| {
                        eprintln!("ledgerfio: --snapshot-dir {dir} cannot be opened ({err})");
                        std::process::exit(2);
                    })
                }),
            },
        )
        .unwrap_or_else(|err| {
            // Refused rather than discovered: every window in the engine is derived from these
            // inputs, so a declaration that does not describe a workload would otherwise become a
            // size nobody meant.
            eprintln!("ledgerfio: the engine's declared capacity is not usable ({err:?})");
            std::process::exit(2);
        });
        let (service, endpoint) = LedgerService::start(
            ServiceConfig {
                reactor: Self::reactor_config(options),
                client_queue: options.client_queue,
                pin: options.pin,
                log_to_stderr: options.log,
                ..ServiceConfig::default()
            },
            Self::open_accounts(workload),
            pending,
            MemoryIdem::start(MemoryIdemConfig {
                latency: options.idem_latency,
                seed: options.seed ^ 0x1de3,
                ..MemoryIdemConfig::default()
            }),
            EchoRaft::start(EchoRaftConfig {
                round_trip: options.raft_round_trip,
                fail_every: options.raft_fail_every,
                seed: options.seed ^ 0x8aff,
                ..EchoRaftConfig::default()
            }),
        )
        // A refused combination is the operator's to fix, not a crash: `validate` exists so it is
        // caught here rather than misbehaving quietly later.
        .unwrap_or_else(|err| {
            eprintln!("ledgerfio: the ledger refused this configuration: {err:?}");
            std::process::exit(2);
        });
        (service, endpoint, calendar)
    }

    fn reactor_config(options: &Options) -> ReactorConfig {
        ReactorConfig {
            batching: BatchPolicy {
                size: options.batch_size,
                max: options.batch_max,
                queued: options.batch_queued.unwrap_or(options.batch_max),
                linger: options.batch_linger,
                in_flight: options.raft_in_flight,
            },
            profile: options.profile,
            ..ReactorConfig::default()
        }
    }

    fn open_accounts(workload: &Workload) -> MemoryAccounts {
        let mut accounts = MemoryAccounts::with_capacity(workload.account_count() as usize + 2);
        accounts.open(EXTERNAL_ACCOUNT, LEDGER, AccountFlags::NONE);
        accounts.open(CLEARING_ACCOUNT, LEDGER, AccountFlags::NONE);
        for index in 0..workload.account_count() {
            accounts.open(
                workload.user_account(index),
                LEDGER,
                AccountFlags::CONSTRAINED,
            );
        }
        accounts
    }
}

/// The engine's calendar, when the run is the one moving it.
///
/// Days rather than a clock, because that is all the engine reads: retention is a calendar promise, and
/// `DaySource` exists so a window measured in days is reachable by something other than waiting. A run
/// that asks for none of this leaves the engine on the wall clock, which is what a load driver measuring
/// throughput should see.
///
/// Starts at day zero on purpose. The engine's cursor starts a lifetime behind whatever day it is first
/// told, so any other origin makes its first sweep a pass over the whole index for a segment that never
/// held anything — a stall belonging to the start of the process rather than to expiry, and one that would
/// sit in the same tail as the thing being measured.
struct Calendar {
    day: Arc<AtomicU64>,
    /// Days to advance across the measured phase, evenly. Zero leaves the engine on the wall clock.
    steps: u64,
}

impl Calendar {
    fn new(steps: u64) -> Self {
        Self {
            day: Arc::new(AtomicU64::new(0)),
            steps,
        }
    }

    fn source(&self) -> DaySource {
        match self.steps {
            0 => DaySource::WallClock,
            _ => DaySource::Fixed(Arc::clone(&self.day)),
        }
    }

    /// Moves the calendar to where the elapsed share of the run puts it. Called from the submit loop, so
    /// the day turns while clients are being served — which is the whole point: a sweep measured on an idle
    /// engine says nothing about what it costs a lookup.
    ///
    /// Spaced over one more interval than there are days, so the last day arrives before the run ends
    /// rather than with it. A boundary crossed at the final instant would start a sweep nothing measures.
    fn advance(&self, elapsed: Duration, duration: Duration) {
        if self.steps == 0 {
            return;
        }
        let share = elapsed.as_secs_f64() / duration.as_secs_f64().max(f64::MIN_POSITIVE);
        let day = (share * (self.steps + 1) as f64) as u64;
        self.day.store(day.min(self.steps), Ordering::Relaxed);
    }
}

/// Completion time of a fixed run of `size` submissions — what a deadline batch actually waits
/// for, as opposed to the latency of one transfer. Groups are cut by transaction id, which the
/// workload hands out in submission order, and held in a fixed ring so tracking one costs an
/// indexed write rather than a hash lookup on the client's hot path.
struct BatchLatency {
    size: u128,
    open: Vec<Group>,
    histogram: Histogram,
}

#[derive(Clone, Copy)]
struct Group {
    id: u64,
    opened_at_nanos: u64,
    acked: u64,
}

const NO_GROUP: u64 = u64::MAX;
const TRACKED_GROUPS: usize = 4096;

impl BatchLatency {
    fn new(size: usize) -> Self {
        Self {
            size: size.max(1) as u128,
            open: vec![
                Group {
                    id: NO_GROUP,
                    opened_at_nanos: 0,
                    acked: 0
                };
                TRACKED_GROUPS
            ],
            histogram: Histogram::new(),
        }
    }

    fn record(&mut self, ack: &Ack, now_nanos: u64) {
        let id = (ack.tx_id.raw() / self.size) as u64;
        let group = &mut self.open[id as usize % TRACKED_GROUPS];
        if group.id != id {
            *group = Group {
                id,
                opened_at_nanos: ack.submitted_at_nanos,
                acked: 0,
            };
        }
        group.opened_at_nanos = group.opened_at_nanos.min(ack.submitted_at_nanos);
        group.acked += 1;
        if group.acked as u128 == self.size {
            let opened = group.opened_at_nanos;
            group.id = NO_GROUP;
            self.histogram.record(now_nanos.saturating_sub(opened));
        }
    }

    fn summary(&self) -> Option<LatencySummary> {
        (self.histogram.count() > 0).then(|| LatencySummary::from(&self.histogram))
    }

    fn reset(&mut self) {
        self.open.fill(Group {
            id: NO_GROUP,
            opened_at_nanos: 0,
            acked: 0,
        });
        self.histogram = Histogram::new();
    }
}

struct Driver {
    client: Client,
    histogram: Histogram,
    batches: BatchLatency,
    batch: Vec<Transfer>,
    submitted: u64,
    acked: u64,
    committed: u64,
    duplicates: u64,
    rejected: u64,
    reject_kinds: BTreeMap<&'static str, u64>,
    /// The ledger has answered a request with `FailStop`, which is it saying it will answer nothing
    /// more. What the drain below exits on, because a request outstanding across a seal is waiting for
    /// a commit that is never coming.
    sealed: bool,
}

impl Driver {
    fn new(client: Client, batch: usize) -> Self {
        Self {
            client,
            histogram: Histogram::new(),
            batches: BatchLatency::new(batch),
            batch: Vec::new(),
            submitted: 0,
            acked: 0,
            committed: 0,
            duplicates: 0,
            rejected: 0,
            reject_kinds: BTreeMap::new(),
            sealed: false,
        }
    }

    /// Balances are created through the normal path, so the measured phase starts from a state
    /// the ledger produced itself.
    fn fund(&mut self, workload: &mut Workload) {
        for index in 0..workload.account_count() {
            if Signals::requested() {
                break;
            }
            let transfer = workload.funding_transfer(index);
            self.push_one(transfer, workload);
        }
        self.drain(workload, false);
        self.reset();
    }

    fn measure(
        &mut self,
        workload: &mut Workload,
        options: &Options,
        calendar: &Calendar,
    ) -> Duration {
        let started = Instant::now();
        // A sealed ledger ends the run: the rest of the requested duration would measure a node that
        // answers everything with `FailStop`, and the report says which it was.
        while started.elapsed() < options.duration && !Signals::requested() && !self.sealed {
            calendar.advance(started.elapsed(), options.duration);
            if self.outstanding() >= options.in_flight {
                self.collect(workload, true);
                continue;
            }
            if options.rate > 0 {
                let target = (started.elapsed().as_secs_f64() * options.rate as f64) as u64;
                if self.submitted >= target {
                    self.collect(workload, true);
                    continue;
                }
            }
            self.push_batch(workload, options.client_batch);
        }
        self.drain(workload, true);
        started.elapsed()
    }

    fn outstanding(&self) -> u64 {
        self.submitted - self.acked
    }

    /// A linked chain must stay inside one submission, so the batch is filled until the
    /// workload closes its chain.
    fn push_batch(&mut self, workload: &mut Workload, size: usize) {
        self.batch.clear();
        while self.batch.len() < size || workload.chain_open() {
            self.batch.push(workload.next());
        }
        let mut offset = 0;
        while offset < self.batch.len() {
            let taken = self.client.submit_batch(&self.batch[offset..]);
            if taken == 0 {
                if self.collect(workload, false) == 0 {
                    std::hint::spin_loop();
                }
                continue;
            }
            self.submitted += taken as u64;
            offset += taken;
        }
    }

    fn push_one(&mut self, transfer: Transfer, workload: &mut Workload) {
        let mut transfer = transfer;
        loop {
            match self.client.submit(transfer) {
                Ok(()) => {
                    self.submitted += 1;
                    return;
                }
                Err(rejected) => {
                    transfer = rejected;
                    if self.collect(workload, false) == 0 {
                        std::hint::spin_loop();
                    }
                }
            }
        }
    }

    fn collect(&mut self, workload: &mut Workload, record: bool) -> usize {
        let mut drained = 0;
        while let Some(ack) = self.client.poll() {
            self.acked += 1;
            drained += 1;
            workload.on_ack(&ack);
            if record {
                self.histogram.record(self.client.latency_nanos(&ack));
                self.batches.record(&ack, self.client.now_nanos());
            }
            self.record_outcome(&ack);
        }
        drained
    }

    /// Collects what the ledger still owes. A second signal is not waited for: an operator who
    /// asks twice wants out now.
    /// Exits on the ledger's terminal state rather than only on a timeout. A sealed apply path means no
    /// commit is ever coming for what is still outstanding — that is the design working — and a loop
    /// that waited for one anyway would spin out its timeout and then report the wrong thing. The
    /// timeout stays for everything that is merely slow.
    fn drain(&mut self, workload: &mut Workload, record: bool) {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while self.outstanding() > 0 && !self.sealed && Instant::now() < deadline {
            if self.collect(workload, record) == 0 {
                std::hint::spin_loop();
            }
        }
    }

    fn record_outcome(&mut self, ack: &Ack) {
        match ack.outcome {
            AckOutcome::Committed => self.committed += 1,
            AckOutcome::Duplicate => self.duplicates += 1,
            AckOutcome::Rejected(err) => {
                self.rejected += 1;
                self.sealed |= err == LedgerError::FailStop;
                *self.reject_kinds.entry(err.name()).or_insert(0) += 1;
            }
        }
    }

    fn reset(&mut self) {
        self.histogram = Histogram::new();
        self.batches.reset();
        self.submitted = 0;
        self.acked = 0;
        self.committed = 0;
        self.duplicates = 0;
        self.rejected = 0;
        self.reject_kinds.clear();
    }
}
