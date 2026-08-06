use std::thread;
use std::time::Duration;

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort};
use ledger_base::{AccountId, Signals};
use ledger_idempotency::{MemoryIdem, MemoryIdemConfig};
use ledger_pending::{MemoryPending, MemoryPendingConfig};
use ledger_raft::{EchoRaft, EchoRaftConfig};
use ledger_service::{ClientEndpoint, LedgerService, ServiceConfig};

const DEFAULT_ACCOUNTS: u64 = 1_000;
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
        MemoryPending::start(MemoryPendingConfig::default())
            .expect("the default engine config is valid"),
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
