use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort};
use ledger_base::{channel, Consumer, Producer};
use ledger_base::{
    AccountId, AcctHandle, Ack, Amount, BudgetGroup, Effect, EffectKind, LinkedChainId, Request,
    Seq, Transfer, TransferFlags, TxId,
};
use ledger_benchkit::{BenchOptions, Samples, STRIDE};
use ledger_idempotency::{MemoryDedup, MemoryDedupConfig};
use ledger_pending::{MemoryPending, MemoryPendingConfig};
use ledger_raft::{EchoRaft, EchoRaftConfig};
use ledger_sequencer::{BatchPolicy, LaneTable, Reactor, ReactorConfig, Transport};
use ledger_stubkit::LatencyRange;

const LEDGER: u32 = 1;
const EXTERNAL: AccountId = AccountId(1);
const REQUESTS: u64 = 2_000_000;
const INLINE_OPS: u64 = 20_000_000;

/// The whole pipeline on one thread with latency-free externals: what the sequencer costs
/// when nothing outside it is in the way.
struct PipelineBench {
    accounts: u64,
}

impl PipelineBench {
    fn run(&self) -> Duration {
        let mut accounts = MemoryAccounts::with_capacity(self.accounts as usize + 1);
        accounts.open(EXTERNAL, LEDGER, AccountFlags::NONE);
        for index in 0..self.accounts {
            accounts.open(AccountId(100 + index), LEDGER, AccountFlags::CONSTRAINED);
        }
        let (request_tx, request_rx) = channel(1 << 14);
        let (ack_tx, ack_rx) = channel(1 << 14);
        let (mut reactor, _events) = Reactor::new(
            ReactorConfig {
                batching: BatchPolicy {
                    size: 1_000,
                    linger: Duration::ZERO,
                    ..ReactorConfig::default().batching
                },
                ..ReactorConfig::default()
            },
            Transport {
                requests: request_rx,
                acks: ack_tx,
            },
            accounts,
            MemoryPending::start(MemoryPendingConfig {
                latency: LatencyRange::fixed(Duration::ZERO),
                ..Default::default()
            })
            .expect("a bench engine config"),
            MemoryDedup::start(MemoryDedupConfig {
                latency: LatencyRange::fixed(Duration::ZERO),
                ..Default::default()
            }),
            EchoRaft::start(EchoRaftConfig {
                round_trip: LatencyRange::fixed(Duration::ZERO),
                ..Default::default()
            }),
        )
        .expect("config");

        let mut driver = Driver {
            requests: request_tx,
            acks: ack_rx,
            submitted: 0,
            acked: 0,
            next_id: 1,
        };
        driver.drive(&mut reactor, REQUESTS / 10, self.accounts);
        let started = Instant::now();
        driver.drive(&mut reactor, REQUESTS, self.accounts);
        started.elapsed()
    }
}

struct Driver {
    requests: Producer<Request>,
    acks: Consumer<Ack>,
    submitted: u64,
    acked: u64,
    next_id: u128,
}

impl Driver {
    const IN_FLIGHT: u64 = 8_192;

    fn drive(
        &mut self,
        reactor: &mut Reactor<MemoryAccounts, MemoryPending, MemoryDedup, EchoRaft>,
        requests: u64,
        accounts: u64,
    ) {
        let target = self.submitted + requests;
        while self.acked < target {
            if self.submitted < target && self.submitted - self.acked < Self::IN_FLIGHT {
                let tx = Transfer {
                    id: TxId(self.next_id),
                    pending_ref: TxId::ABSENT,
                    debit_account: EXTERNAL,
                    credit_account: AccountId(100 + self.submitted.wrapping_mul(STRIDE) % accounts),
                    amount: 1,
                    ledger: LEDGER,
                    flags: TransferFlags::NONE,
                };
                if self.requests.push(Request::single(tx, 0)).is_ok() {
                    self.next_id += 1;
                    self.submitted += 1;
                }
            }
            reactor.tick();
            while self.acks.pop().is_some() {
                self.acked += 1;
            }
        }
    }
}

/// Seq issue plus the contract-1 check on the sequencer's own lane state.
struct LaneBench {
    accounts: usize,
}

impl LaneBench {
    fn run(&self) -> Duration {
        let mut lanes = LaneTable::with_capacity(self.accounts);
        lanes.get_mut(AcctHandle::new(self.accounts - 1));
        let started = Instant::now();
        for step in 0..INLINE_OPS {
            let index = (step.wrapping_mul(STRIDE) % self.accounts as u64) as usize;
            let lane = lanes.get_mut(AcctHandle::new(index));
            let seq = lane.issue_seq();
            let _ = black_box(lane.accept_seq(seq));
        }
        started.elapsed()
    }
}

/// What the ownership split costs on the inline path. Lane state belongs to the sequencer and
/// the columns belong to the account component, so a judge plus apply touches two cache lines;
/// the fused variant shows the price of that separation. Effects are walked in order, as a
/// committed batch is, and the accounts they name are scattered.
struct SplitBench {
    accounts: usize,
    fused: bool,
}

ledger_base::cache_aligned! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct FusedEntry {
        pub seq_counter: Seq,
        pub last_seq: Seq,
        pub debits_posted: Amount,
        pub credits_posted: Amount,
        pub debits_pending: Amount,
        pub credits_pending: Amount,
    }
}

impl SplitBench {
    fn run(&self) -> Duration {
        let mut accounts = MemoryAccounts::with_capacity(self.accounts);
        for index in 0..self.accounts {
            accounts.open(AccountId(index as u64 + 1), LEDGER, AccountFlags::NONE);
        }
        let mut lanes = LaneTable::with_capacity(self.accounts);
        lanes.get_mut(AcctHandle::new(self.accounts - 1));
        let mut fused = vec![FusedEntry::default(); self.accounts];
        let effects: Vec<Effect> = (0..self.accounts)
            .map(|step| {
                let index = (step as u64).wrapping_mul(STRIDE) as usize % self.accounts;
                Effect {
                    tx_id: TxId(index as u128 + 1),
                    pending_ref: TxId::ABSENT,
                    debit_account: AccountId(index as u64 + 1),
                    credit_account: AccountId(index as u64 + 1),
                    amount: 1,
                    remaining_after: 0,
                    debit: AcctHandle::new(index),
                    credit: AcctHandle::new(index),
                    chain: LinkedChainId::ABSENT,
                    budget: BudgetGroup::ABSENT,
                    ledger: LEDGER,
                    kind: EffectKind::Post,
                }
            })
            .collect();

        let started = Instant::now();
        for step in 0..INLINE_OPS {
            let effect = &effects[step as usize % effects.len()];
            let index = effect.debit.index();
            if self.fused {
                let entry = &mut fused[index];
                entry.seq_counter += 1;
                let _ = black_box(entry.seq_counter == entry.last_seq + 1);
                entry.last_seq = entry.seq_counter;
                entry.debits_posted += 1;
                entry.credits_posted += 1;
            } else {
                let lane = lanes.get_mut(AcctHandle::new(index));
                let seq = lane.issue_seq();
                let _ = black_box(lane.accept_seq(seq));
                let _ = black_box(accounts.apply(effect));
            }
        }
        started.elapsed()
    }
}

/// The lane is touched three times per request (issue, check, overlay), so it is the state most
/// worth getting right. A whole cache line per lane never straddles but costs four times the
/// memory; 32 bytes never straddles either, because 32 divides both line sizes.
struct LaneLayoutBench {
    accounts: usize,
    line_aligned: bool,
}

/// The fields are the point: they are the bytes being measured.
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct SnugLane {
    seq_counter: u64,
    last_seq: u64,
    speculative: i64,
    in_flight: u32,
    pending_replies: u16,
    quarantined: bool,
}

ledger_base::cache_aligned! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct AlignedLane {
        pub seq_counter: u64,
        pub last_seq: u64,
        pub speculative: i64,
        pub in_flight: u32,
        pub pending_replies: u16,
        pub quarantined: bool,
    }
}

impl LaneLayoutBench {
    fn run(&self) -> Duration {
        let mut snug = vec![SnugLane::default(); if self.line_aligned { 0 } else { self.accounts }];
        let mut aligned =
            vec![AlignedLane::default(); if self.line_aligned { self.accounts } else { 0 }];
        let started = Instant::now();
        for step in 0..INLINE_OPS {
            let index = (step.wrapping_mul(STRIDE) % self.accounts as u64) as usize;
            if self.line_aligned {
                let lane = &mut aligned[index];
                lane.seq_counter += 1;
                black_box(lane.seq_counter == lane.last_seq + 1);
                lane.last_seq = lane.seq_counter;
                lane.speculative -= 1;
            } else {
                let lane = &mut snug[index];
                lane.seq_counter += 1;
                black_box(lane.seq_counter == lane.last_seq + 1);
                lane.last_seq = lane.seq_counter;
                lane.speculative -= 1;
            }
        }
        started.elapsed()
    }
}

/// The slot pool is reached by slot id, so its entries are touched at random, four or five times
/// per request. 112 bytes straddles a line; 128 never does. The question is whether the extra
/// memory costs more than the straddle saves.
struct SlotLayoutBench {
    slots: usize,
    padded: bool,
}

/// The fields are the point: they are the bytes being measured.
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct SlotBody {
    tx: [u64; 8],
    digest: u64,
    seq: u64,
    lane: u64,
    handles: u64,
    tail: u64,
}

#[derive(Clone, Copy, Default)]
#[repr(align(128))]
struct PaddedSlot(SlotBody);

impl SlotLayoutBench {
    fn run(&self) -> Duration {
        let mut packed = vec![SlotBody::default(); if self.padded { 0 } else { self.slots }];
        let mut padded = vec![PaddedSlot::default(); if self.padded { self.slots } else { 0 }];
        let started = Instant::now();
        for step in 0..INLINE_OPS {
            let index = (step.wrapping_mul(STRIDE) % self.slots as u64) as usize;
            // Four touches, the way a request visits its slot: prepare, dispatch, judge, finish.
            for _ in 0..4 {
                let body = if self.padded {
                    &mut padded[index].0
                } else {
                    &mut packed[index]
                };
                body.seq += 1;
                body.tail += body.digest;
                black_box(body.lane);
            }
        }
        started.elapsed()
    }
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();

    for accounts in [1_000u64, 1_000_000] {
        let bench = PipelineBench { accounts };
        let mut samples =
            Samples::new(format!("pipeline single-phase ({accounts} acct)"), REQUESTS);
        for _ in 0..options.repeat {
            samples.add(bench.run());
        }
        samples.report();
    }

    for accounts in [1_000usize, 1_000_000, 8_000_000] {
        for line_aligned in [false, true] {
            let bench = LaneLayoutBench {
                accounts,
                line_aligned,
            };
            let layout = if line_aligned {
                "line-aligned"
            } else {
                "32B snug"
            };
            let mut samples = Samples::new(format!("lane {layout} ({accounts} acct)"), INLINE_OPS);
            for _ in 0..options.repeat {
                samples.add(bench.run());
            }
            samples.report();
        }
    }

    for slots in [4_096usize, 65_536, 1_000_000] {
        for padded in [false, true] {
            let bench = SlotLayoutBench { slots, padded };
            let layout = if padded { "128B padded" } else { "112B packed" };
            let mut samples = Samples::new(format!("slot {layout} ({slots} slots)"), INLINE_OPS);
            for _ in 0..options.repeat {
                samples.add(bench.run());
            }
            samples.report();
        }
    }

    for accounts in [1_000usize, 100_000, 1_000_000, 8_000_000] {
        let bench = LaneBench { accounts };
        let mut samples = Samples::new(format!("lane seq issue + check ({accounts})"), INLINE_OPS);
        for _ in 0..options.repeat {
            samples.add(bench.run());
        }
        samples.report();
    }

    for accounts in [1_000usize, 1_000_000, 8_000_000] {
        for fused in [false, true] {
            let bench = SplitBench { accounts, fused };
            let layout = if fused { "fused" } else { "split (current)" };
            let mut samples = Samples::new(format!("lane+apply {layout} ({accounts})"), INLINE_OPS);
            for _ in 0..options.repeat {
                samples.add(bench.run());
            }
            samples.report();
        }
    }
}
