//! A deterministic performance test: the reactor allocates nothing per request once it is warm.
//! It counts allocations on its own thread, so it drives the reactor directly instead of using the
//! shared harness, which allocates while collecting acks.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use ledger_account::MemoryAccounts;
use ledger_base::ports::AccountFlags;
use ledger_base::{Ack, AccountId, Request, Transfer, TransferFlags, TxId};
use ledger_stubkit::LatencyRange;
use ledger_idempotency::{MemoryDedup, MemoryDedupConfig};
use ledger_pending::{MemoryPending, MemoryPendingConfig};
use ledger_raft::{EchoRaft, EchoRaftConfig};
use ledger_base::{channel, Consumer, Producer};
use ledger_sequencer::{BatchPolicy, Reactor, ReactorConfig, Transport};

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// Counts allocations per thread, so the reactor's own behaviour can be measured while
/// the stub threads keep allocating freely.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const LEDGER: u32 = 1;
const EXTERNAL: AccountId = AccountId(1);
const ACCOUNTS: u64 = 64;
const IN_FLIGHT: u64 = 2_048;

struct Load {
    reactor: Reactor<MemoryAccounts, MemoryPending, MemoryDedup, EchoRaft>,
    requests: Producer<Request>,
    acks: Consumer<Ack>,
    next_id: u128,
    submitted: u64,
    acked: u64,
}

impl Load {
    fn new() -> Self {
        let mut accounts = MemoryAccounts::with_capacity(ACCOUNTS as usize + 1);
        accounts.open(EXTERNAL, LEDGER, AccountFlags::NONE);
        for index in 0..ACCOUNTS {
            accounts.open(AccountId(100 + index), LEDGER, AccountFlags::CONSTRAINED);
        }
        let (request_tx, request_rx) = channel(1 << 14);
        let (ack_tx, ack_rx) = channel(1 << 14);
        let (reactor, _events) = Reactor::new(
            ReactorConfig {
                batching: BatchPolicy {
                    size: 256,
                    linger: Duration::ZERO,
                    ..ReactorConfig::default().batching
                },
                ..ReactorConfig::default()
            },
            Transport { requests: request_rx, acks: ack_tx },
            accounts,
            MemoryPending::start(MemoryPendingConfig {
                latency: LatencyRange::fixed(Duration::ZERO),
                ..MemoryPendingConfig::default()
            }),
            MemoryDedup::start(MemoryDedupConfig {
                latency: LatencyRange::fixed(Duration::ZERO),
                ..MemoryDedupConfig::default()
            }),
            EchoRaft::start(EchoRaftConfig {
                round_trip: LatencyRange::fixed(Duration::ZERO),
                ..EchoRaftConfig::default()
            }),
        )
        .expect("config");
        Self { reactor, requests: request_tx, acks: ack_rx, next_id: 1, submitted: 0, acked: 0 }
    }

    fn run(&mut self, requests: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let target = self.submitted + requests;
        while self.acked < target {
            if self.submitted < target && self.submitted - self.acked < IN_FLIGHT {
                let id = TxId(self.next_id);
                self.next_id += 1;
                let transfer = Transfer {
                    id,
                    pending_ref: TxId::ABSENT,
                    debit_account: EXTERNAL,
                    credit_account: AccountId(100 + self.submitted % ACCOUNTS),
                    amount: 1,
                    ledger: LEDGER,
                    flags: TransferFlags::NONE,
                };
                if self.requests.push(Request::single(transfer, 0)).is_ok() {
                    self.submitted += 1;
                }
            }
            self.reactor.tick();
            while self.acks.pop().is_some() {
                self.acked += 1;
            }
            assert!(Instant::now() < deadline, "pipeline stalled");
        }
    }
}

#[test]
fn the_steady_state_pipeline_allocates_nothing_per_request() {
    let mut load = Load::new();
    load.run(4_000);

    let before = ALLOCATIONS.with(|count| count.get());
    load.run(40_000);
    let allocations = ALLOCATIONS.with(|count| count.get()) - before;

    assert_eq!(allocations, 0, "40k requests must not allocate on the reactor thread");
}
