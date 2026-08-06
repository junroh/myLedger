//! Shared driver for the integration tests. Cargo builds every `tests/*.rs` as its own binary and
//! everything under `tests/*/` as shared code, so this is where the setup lives.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort, AccountRecord};
use ledger_base::{
    channel, AccountId, Ack, AckOutcome, Amount, Clock, Consumer, Effect, LogStream, ManualClock,
    Producer, Request, SystemClock, Transfer, TransferFlags, TxId,
};
use ledger_idempotency::{MemoryIdem, MemoryIdemConfig};
use ledger_pending::{
    DaySource, MemoryPending, MemoryPendingConfig, OpenBacking, PendingCapacity, StoreModel,
};
use ledger_raft::{EchoRaft, EchoRaftConfig};
use ledger_sequencer::{BatchPolicy, Reactor, ReactorConfig, Transport};
use ledger_stubkit::LatencyRange;

pub const LEDGER: u32 = 1;
pub const EXTERNAL: AccountId = AccountId(1);
pub const ALICE: AccountId = AccountId(10);
pub const BOB: AccountId = AccountId(11);
pub const POINTS: AccountId = AccountId(12);
pub const DEPOSIT: AccountId = AccountId(13);
pub const FUNDING: Amount = 1_000;
/// Every account the harness opens, so a check can walk all of them.
pub const ACCOUNTS: [AccountId; 5] = [EXTERNAL, ALICE, BOB, POINTS, DEPOSIT];

const TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_QUEUE: usize = 1 << 12;

pub type TestReactor<C = SystemClock> =
    Reactor<MemoryAccounts, MemoryPending, MemoryIdem, EchoRaft, C>;

/// Zero-latency stubs, so a functional test observes decisions rather than timing.
pub struct NoLatency;

impl NoLatency {
    pub fn pending() -> MemoryPendingConfig {
        MemoryPendingConfig {
            latency: LatencyRange::fixed(Duration::ZERO),
            ..MemoryPendingConfig::default()
        }
    }

    /// Keeps nothing in the overlay, so every resolution goes out as a lookup. A test about the
    /// pending path needs that; a hold the engine has just created answers inline otherwise.
    pub fn cold_pending() -> MemoryPendingConfig {
        MemoryPendingConfig {
            overlay_soft_limit: 0,
            eviction_per_round: 1024,
            ..Self::pending()
        }
    }

    /// A store that refuses every call, which is the only way to reach the seal a device fault produces:
    /// nothing else can make `MemoryStore` fail. Short windows beside it, so a run of a few holds actually
    /// reaches the store rather than resolving everything out of the writeback buffer.
    pub fn failing_store() -> MemoryPendingConfig {
        MemoryPendingConfig {
            // Windows short enough that a handful of holds seals a block, and an index wide enough that it
            // does not fill first — otherwise `HoldNotStored` fires before the store is ever called, which is
            // the other seal and not this one.
            capacity: PendingCapacity {
                daily_arrivals: 2_400,
                ..Self::short_windows()
            },
            store: StoreModel {
                fault_every: 1,
                queue_depth: 8,
                ..StoreModel::default()
            },
            ..Self::pending()
        }
    }

    /// A store that answers every read with a bit changed. The other half of `failing_store`: one refuses and
    /// this one lies, and only the second was ever a wrong answer rather than a stop.
    pub fn corrupting_store() -> MemoryPendingConfig {
        MemoryPendingConfig {
            store: StoreModel {
                corrupt_every: 1,
                // Off, or the refusal fires first and the test proves the other seal.
                fault_every: 0,
                ..Self::failing_store().store
            },
            // Nothing kept in the overlay, so a resolution has to ask the engine and the read reaches the
            // store rather than being answered from what the sequencer already decided.
            overlay_soft_limit: 0,
            eviction_per_round: 1024,
            ..Self::failing_store()
        }
    }

    /// The smallest declared maximum the engine will accept, which sizes its index at a couple of
    /// dozen slots. A test can then pass that maximum in a handful of holds instead of a load run —
    /// and passing it is the one thing the engine reports without being asked.
    pub fn tiny_index() -> MemoryPendingConfig {
        MemoryPendingConfig {
            capacity: Self::short_windows(),
            ..Self::pending()
        }
    }

    /// A flush window of one block, so a test reaches the store — and therefore a real segment — in one
    /// block's worth of holds rather than an hour of arrivals. Retention is one day plus the grace that
    /// keeps deletion from being early, which is the smallest lifetime the engine will accept.
    pub fn short_windows() -> PendingCapacity {
        PendingCapacity {
            daily_arrivals: 24,
            worst_survivor_share: 0.5,
            retention_days: 1,
            grace_days: 1,
            survives_flush_window: 0.5,
            flush_window_hours: 1,
            residency_hours: 1,
        }
    }

    /// Windows short enough that a test's holds reach a segment, and an index roomy enough that they do
    /// not overflow it on the way: a test about expiry must not be a test about the seal.
    ///
    /// One number decides both, which is the point of deriving every size from declared inputs — so it is
    /// chosen rather than tuned. A flush window of one block needs an hour's arrivals to be one block's
    /// worth of records; the index is then sized for a lifetime of those, which is far more holds than a
    /// test writes.
    pub fn expiring() -> MemoryPendingConfig {
        let hourly_records = ledger_pending::RECORDS_PER_BLOCK as u64;
        MemoryPendingConfig {
            capacity: PendingCapacity {
                daily_arrivals: hourly_records * 24,
                ..Self::short_windows()
            },
            ..Self::pending()
        }
    }

    pub fn idem() -> MemoryIdemConfig {
        MemoryIdemConfig {
            latency: LatencyRange::fixed(Duration::ZERO),
            ..MemoryIdemConfig::default()
        }
    }

    pub fn raft() -> EchoRaftConfig {
        EchoRaftConfig {
            round_trip: LatencyRange::fixed(Duration::ZERO),
            ..EchoRaftConfig::default()
        }
    }
}

/// Drives the reactor on the test thread, so every assertion sees a settled state.
pub struct Harness<C = SystemClock> {
    pub reactor: TestReactor<C>,
    requests: Producer<Request>,
    acks: Consumer<Ack>,
    log: LogStream,
    next_id: u128,
    /// The day the engine thinks it is. Retention is measured in days, so a test that had to wait for one
    /// to pass could not reach expiry at all.
    day: Arc<AtomicU64>,
}

impl Harness<SystemClock> {
    pub fn new() -> Self {
        Self::with_stubs(NoLatency::pending(), NoLatency::raft())
    }

    pub fn with_stubs(pending: MemoryPendingConfig, raft: EchoRaftConfig) -> Self {
        Self::with_config(ReactorConfig::default(), pending, raft)
    }

    pub fn with_config(
        config: ReactorConfig,
        pending: MemoryPendingConfig,
        raft: EchoRaftConfig,
    ) -> Self {
        Self::build(
            Self::eager(config),
            pending,
            raft,
            SystemClock::new(),
            CLIENT_QUEUE,
        )
    }

    /// A shallow client queue, so a client that stops reading its acks becomes backpressure in a
    /// handful of requests instead of thousands.
    pub fn with_client_queue(config: ReactorConfig, queue: usize) -> Self {
        let config = Self::eager(config);
        Self::build(
            config,
            NoLatency::pending(),
            NoLatency::raft(),
            SystemClock::new(),
            queue,
        )
    }

    /// One effect per batch and no linger, so a request is decided by the tick it is judged in.
    fn eager(config: ReactorConfig) -> ReactorConfig {
        ReactorConfig {
            batching: BatchPolicy {
                size: 1,
                linger: Duration::ZERO,
                ..config.batching
            },
            ..config
        }
    }
}

impl Harness<ManualClock> {
    /// For anything that depends on time passing: the test advances the clock itself.
    pub fn with_clock(batching: BatchPolicy, clock: ManualClock) -> Self {
        let config = ReactorConfig {
            batching,
            ..ReactorConfig::default()
        };
        Self::build(
            config,
            NoLatency::pending(),
            NoLatency::raft(),
            clock,
            CLIENT_QUEUE,
        )
    }
}

impl<C: Clock> Harness<C> {
    fn build(
        config: ReactorConfig,
        pending: MemoryPendingConfig,
        raft: EchoRaftConfig,
        clock: C,
        client_queue: usize,
    ) -> Self {
        let mut accounts = MemoryAccounts::with_capacity(8);
        accounts.open(EXTERNAL, LEDGER, AccountFlags::NONE);
        for account in [ALICE, BOB, POINTS, DEPOSIT] {
            accounts.open(account, LEDGER, AccountFlags::CONSTRAINED);
        }
        let (request_tx, request_rx) = channel(client_queue);
        let (ack_tx, ack_rx) = channel(client_queue);
        let (days, day) = DaySource::manual(0);
        let (reactor, log) = Reactor::with_clock(
            config,
            Transport {
                requests: request_rx,
                acks: ack_tx,
            },
            accounts,
            MemoryPending::start_with_days(pending, days, OpenBacking::Memory)
                .expect("a test engine config"),
            MemoryIdem::start(NoLatency::idem()),
            EchoRaft::start(raft),
            clock,
        )
        .expect("config");
        Self {
            reactor,
            requests: request_tx,
            acks: ack_rx,
            log,
            next_id: 1,
            day,
        }
    }

    /// Moves the engine's day on, which is the whole of what a clock does for retention.
    pub fn open_day(&self, day: u64) {
        self.day.store(day, Ordering::Relaxed);
    }

    /// Waits until the engine has written `records` of them out of its writeback buffer.
    ///
    /// A record's segment is the day it was **written**, not the day its hold was committed, and writing
    /// happens on the engine's own thread. So a test that moves the day before the writes have landed puts
    /// those records in the *next* day and then waits for an expiry that is a day further out than it
    /// thinks. Establishing the precondition is the test's job, not the engine's.
    pub fn tick_until_written(&mut self, records: u64) {
        self.tick_until("the engine never wrote the records out", |reactor| {
            reactor.pending().traffic().flushed >= records
        });
    }

    /// What one account still has reserved, readable from inside a `tick_until` predicate — which is where
    /// a test about expiry has to wait, because there is no ack to wait for.
    pub fn pending_column(reactor: &TestReactor<C>) -> Amount {
        let handle = reactor.accounts().resolve(ALICE).expect("a known account");
        reactor.accounts().record(handle).debits_pending()
    }

    pub fn transfer(&mut self, debit: AccountId, credit: AccountId, amount: Amount) -> Transfer {
        let id = TxId(self.next_id);
        self.next_id += 1;
        Transfer {
            id,
            pending_ref: TxId::ABSENT,
            debit_account: debit,
            credit_account: credit,
            amount,
            ledger: LEDGER,
            flags: TransferFlags::NONE,
        }
    }

    /// Every leg but the last carries the linked flag, which is what terminates the chain.
    pub fn linked_leg(
        &mut self,
        debit: AccountId,
        credit: AccountId,
        amount: Amount,
        last: bool,
    ) -> Transfer {
        let mut tx = self.transfer(debit, credit, amount);
        if !last {
            tx.flags = TransferFlags::LINKED;
        }
        tx
    }

    pub fn submit(&self, tx: Transfer) {
        self.requests
            .push(Request::single(tx, 0))
            .expect("client queue");
    }

    /// A chain is one submission, so only its last leg ends the batch.
    pub fn submit_chain(&self, legs: &[Transfer]) {
        for (index, leg) in legs.iter().enumerate() {
            let request = Request {
                tx: *leg,
                submitted_at_nanos: 0,
                end_of_batch: index + 1 == legs.len(),
            };
            self.requests.push(request).expect("client queue");
        }
    }

    pub fn poll(&self) -> Option<Ack> {
        self.acks.pop()
    }

    /// Submits one transfer and ticks until it is acked.
    pub fn run(&mut self, tx: Transfer) -> Ack {
        self.submit(tx);
        self.drain_acks(1, "no ack").remove(0)
    }

    /// Submits a chain and ticks until every leg is acked.
    pub fn run_chain(&mut self, legs: &[Transfer]) -> Vec<Ack> {
        self.submit_chain(legs);
        self.drain_acks(legs.len(), "chain stalled")
    }

    /// A timeout carries the counters with it: what the ledger was doing when it stopped making progress is
    /// the whole of what a stalled test has to say, and a bare reason string says none of it.
    pub fn tick_until(&mut self, reason: &str, mut done: impl FnMut(&TestReactor<C>) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.reactor) {
            self.reactor.tick();
            assert!(
                Instant::now() < deadline,
                "{reason}\n  {}\n  {:?}",
                self.lanes_now(),
                self.reactor.metrics()
            );
        }
    }

    /// The lanes a test moves money on, for the message above. Counters say a run stopped; only this says
    /// whether it stopped because a lane is stuck — a seq issued and never judged holds up everything
    /// behind it, and no counter distinguishes that from a lane with nothing left to do.
    fn lanes_now(&self) -> String {
        [ALICE, BOB]
            .iter()
            .filter_map(|&account| self.reactor.lane(account).map(|lane| (account, lane)))
            .map(|(account, lane)| {
                format!(
                    "sealed={} lane {account:?} last_seq={} in_flight={} awaits_pending={} quarantined={}",
                    self.reactor.applies_sealed(),
                    lane.last_seq(),
                    lane.in_flight(),
                    lane.awaits_pending_reply(),
                    lane.is_quarantined()
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ")
    }

    /// Ticks until `wanted` acks have come back, whatever they say.
    pub fn drain_acks(&mut self, wanted: usize, reason: &str) -> Vec<Ack> {
        let deadline = Instant::now() + TIMEOUT;
        let mut acks = Vec::with_capacity(wanted);
        while acks.len() < wanted {
            self.reactor.tick();
            while let Some(ack) = self.poll() {
                acks.push(ack);
            }
            assert!(Instant::now() < deadline, "{reason}");
        }
        acks
    }

    pub fn fund(&mut self, account: AccountId, amount: Amount) {
        let tx = self.transfer(EXTERNAL, account, amount);
        assert_eq!(
            self.run(tx).outcome,
            AckOutcome::Committed,
            "funding {account:?}"
        );
    }

    pub fn hold(&mut self, debit: AccountId, credit: AccountId, amount: Amount) -> (TxId, Ack) {
        let mut tx = self.transfer(debit, credit, amount);
        tx.flags = TransferFlags::PENDING;
        (tx.id, self.run(tx))
    }

    pub fn resolve(
        &mut self,
        hold: TxId,
        debit: AccountId,
        credit: AccountId,
        amount: Amount,
        flags: TransferFlags,
    ) -> Ack {
        let mut tx = self.transfer(debit, credit, amount);
        tx.pending_ref = hold;
        tx.flags = flags;
        self.run(tx)
    }

    /// Settles several holds as one chain, which is how a budget group is resolved.
    pub fn resolve_together(&mut self, legs: &[(TxId, AccountId, AccountId, Amount)]) -> Vec<Ack> {
        let last = legs.len() - 1;
        let chain: Vec<Transfer> = legs
            .iter()
            .enumerate()
            .map(|(index, (hold, debit, credit, amount))| {
                let mut tx = self.transfer(*debit, *credit, *amount);
                tx.pending_ref = *hold;
                tx.flags = TransferFlags::POST_PENDING;
                if index != last {
                    tx.flags = tx.flags.with(TransferFlags::LINKED);
                }
                tx
            })
            .collect();
        self.run_chain(&chain)
    }

    /// Debits posted, credits posted, debits pending, credits pending.
    pub fn columns(&self, account: AccountId) -> (Amount, Amount, Amount, Amount) {
        let record = self.record(account);
        (
            record.debits_posted(),
            record.credits_posted(),
            record.debits_pending(),
            record.credits_pending(),
        )
    }

    /// The ledger's own audit, plus the one thing it cannot reach generically: a column below zero in
    /// some single account, which the sums would hide. Any test that moves money ends here.
    pub fn assert_consistent(&self) {
        assert_eq!(self.reactor.audit(), Ok(()), "the ledger's own audit");
        assert_eq!(
            self.reactor.metrics().invariant_breaks,
            0,
            "an invariant broke during the run"
        );
        for account in ACCOUNTS {
            let columns = self.columns(account);
            let (debits, credits, debits_pending, credits_pending) = columns;
            assert!(
                debits >= 0 && credits >= 0 && debits_pending >= 0 && credits_pending >= 0,
                "{account:?} has a negative column: {columns:?}"
            );
        }
    }

    pub fn available(&self, account: AccountId) -> Amount {
        self.record(account).available()
    }

    pub fn record(&self, account: AccountId) -> &AccountRecord {
        let handle = self
            .reactor
            .accounts()
            .resolve(account)
            .expect("known account");
        self.reactor.accounts().record(handle)
    }

    pub fn raft_log(&self) -> Vec<Effect> {
        self.reactor.raft().log()
    }

    pub fn logged(&self, kind: u16) -> bool {
        let mut found = false;
        while let Some(event) = self.log.poll() {
            found |= event.kind == kind;
        }
        found
    }
}
