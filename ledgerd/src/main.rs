use std::thread;
use std::time::Duration;

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort};
use ledger_base::{AccountId, Signals};
use ledger_idempotency::{MemoryIdem, MemoryIdemConfig};
use ledger_pending::{
    DaySource, MemoryPending, MemoryPendingConfig, OpenBacking, PendingStorage, SnapshotPolicy,
};
use ledger_raft::{EchoRaft, EchoRaftConfig};
use ledger_service::{ClientEndpoint, LedgerService, ServiceConfig};

const DEFAULT_ACCOUNTS: u64 = 1_000;
/// Threads issuing the store's `pread`s. Zero reads synchronously, which is what is verified: a pool pays for
/// itself only where a read blocks, and without `O_DIRECT` every read is a page-cache hit — measured, it costs
/// a third of the read ceiling and buys nothing. A deployment that bypasses the cache sets this from Little's
/// law on the store read rate; design notes §16 has the numbers and why the design's sixteen is not the
/// default here.
const READ_THREADS: usize = 0;
/// Whether `pwrite` and `fsync` go to a thread of their own. **On**, because every measurement of it is
/// one-sided: p99.9 goes from 103–124ms to 3.4–3.7ms, throughput at saturation is +37%, and a snapshot's
/// cost to the median falls from 13–29% to 4%. One thread, and it has to be one — writes do not commute.
/// `--store-write-lane 0` is still the synchronous baseline, which is what the older numbers in the
/// documents were taken against. Design notes §20.
const WRITE_LANE: bool = true;
/// Log positions between snapshots when a directory was named and no distance was. Small enough that a
/// local run writes several, which is what makes the flag observable at all; a deployment says its own,
/// and what it should be is arithmetic on the log it means to retain — see design notes §19.
const SNAPSHOT_EVERY: u64 = 100_000;
const EXTERNAL: AccountId = AccountId(1);
const FIRST_ACCOUNT: u64 = 1_000;
const LEDGER: u32 = 1;

fn main() {
    let accounts = account_count();
    let started = LedgerService::start(
        ServiceConfig {
            log_to_stderr: true,
            ..ServiceConfig::default()
        },
        open_accounts(accounts),
        start_pending(),
        MemoryIdem::start(MemoryIdemConfig::default()),
        EchoRaft::start(EchoRaftConfig::default()),
    );
    let (service, endpoint) = match started {
        Ok(started) => started,
        Err(err) => {
            eprintln!("ledgerd: cannot start: {err}");
            std::process::exit(1);
        }
    };

    Signals::install();
    serve(endpoint);

    let Some(stopped) = service.shutdown() else {
        eprintln!("ledgerd: the reactor did not stop cleanly");
        std::process::exit(1);
    };
    let metrics = stopped.reactor.metrics();
    let totals = stopped.reactor.accounts().totals();
    eprintln!(
        "ledgerd: stopped drained={} committed={} rejected={} gaps={} posted={}=={} pending={}=={}",
        stopped.drained,
        metrics.committed,
        metrics.rejected,
        metrics.seq_gaps,
        totals.debits_posted,
        totals.credits_posted,
        totals.debits_pending,
        totals.credits_pending
    );
}

/// Where a network listener attaches: one `ClientEndpoint` per connection. Until it exists the
/// process serves nobody and `ledgerfio` is how the ledger is driven.
fn serve(_endpoint: ClientEndpoint) {
    while !Signals::requested() {
        thread::sleep(Duration::from_millis(50));
    }
}

/// The engine, on files if a directory was named and in memory otherwise.
///
/// Memory is the default here as it is everywhere: it is what every number in the documents was taken
/// against, and a node that wrote files without being asked would change what a run means. A directory is
/// refused loudly rather than fallen back from — somebody asked for durable space and got none is worse than
/// not starting.
///
/// The two directories are separate flags because they may be separate volumes (§19); naming neither is
/// still the default, and naming one without the other is a perfectly ordinary deployment.
fn start_pending() -> MemoryPending {
    let blocks = match flag("--store-dir") {
        None => OpenBacking::Memory,
        Some(dir) => OpenBacking::files(std::path::Path::new(&dir), READ_THREADS, WRITE_LANE)
            .unwrap_or_else(|err| {
                eprintln!("ledgerd: --store-dir {dir} cannot be opened ({err:?})");
                std::process::exit(2);
            }),
    };
    let snapshots = flag("--snapshot-dir").map(|dir| {
        // A directory is a volume: naming the one the blocks are on is what declares the two one disk,
        // and then one store serves both because there is one device to queue on (§20).
        OpenBacking::files(std::path::Path::new(&dir), 0, WRITE_LANE).unwrap_or_else(|err| {
            eprintln!("ledgerd: --snapshot-dir {dir} cannot be opened ({err:?})");
            std::process::exit(2);
        })
    });
    // A directory with no cadence writes nothing, which reads as a flag that did not work. The two have
    // to arrive together, so naming the directory is what turns the policy on.
    let snapshot = SnapshotPolicy {
        every: match snapshots {
            None => 0,
            Some(_) => number("--snapshot-every").unwrap_or(SNAPSHOT_EVERY),
        },
        ..SnapshotPolicy::default()
    };
    MemoryPending::start_with_days(
        MemoryPendingConfig {
            snapshot,
            ..MemoryPendingConfig::default()
        },
        DaySource::WallClock,
        PendingStorage { blocks, snapshots },
    )
    .expect("the default engine config is valid")
}

fn number(name: &str) -> Option<u64> {
    flag(name).and_then(|value| value.parse().ok())
}

fn flag(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}

fn account_count() -> u64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--accounts" {
            return args
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_ACCOUNTS);
        }
    }
    DEFAULT_ACCOUNTS
}

fn open_accounts(count: u64) -> MemoryAccounts {
    let mut accounts = MemoryAccounts::with_capacity(count as usize + 1);
    accounts.open(EXTERNAL, LEDGER, AccountFlags::NONE);
    for index in 0..count {
        accounts.open(
            AccountId(FIRST_ACCOUNT + index),
            LEDGER,
            AccountFlags::CONSTRAINED,
        );
    }
    accounts
}
