//! The ledger's own work goes behind the work someone is waiting for.
//!
//! An expiry void is an input the ledger sends itself: nobody asked for it, nothing is waiting on its ack,
//! and one that is lost is offered again from the slice the engine keeps. A client's request is none of those
//! things. So the two arrive on separate paths and are read in that order — clients first, and the voids on
//! a budget of their own.
//!
//! Being a *budget* rather than a threshold is what keeps this safe in the other direction: a share of every
//! tick that admits anything is never a share of nothing, so traffic that never lets up still cannot starve
//! expiry to a standstill. A hold that is never released stays in an index that never grows, and that ends
//! in the seal — so yielding to clients may not mean yielding entirely.
//!
//! What is covered here is the order, under a slot pool scarce enough for the order to decide who gets one.
//! The per-tick bound itself is not: how many voids are waiting when a tick begins depends on the engine's
//! own thread, so a test asserting the bound passes whether the bound is there or not. `ExpiryQueue`'s unit
//! test covers what that queue promises, and `ledgersim` reports the churn.

mod harness;

use ledger_base::{AckOutcome, Amount, TransferFlags};
use ledger_pending::RECORDS_PER_BLOCK;
use ledger_sequencer::{Capacity, ReactorConfig};

use harness::*;

const HOLDS: usize = RECORDS_PER_BLOCK + 1;
const AMOUNT: Amount = 5;
const LIFETIME: u64 = 2;
const WRITTEN: u64 = (HOLDS - HOLDS % RECORDS_PER_BLOCK) as u64;

/// Fewer slots than the engine offers voids in one slice, so whichever path is read first takes them all.
/// That is what makes the order observable from outside: with a roomy pool both are served either way.
const SLOTS: usize = 8;

fn expiring_with(capacity: Capacity) -> Harness {
    let config = ReactorConfig {
        capacity,
        ..ReactorConfig::default()
    };
    let mut harness = Harness::with_config(config, NoLatency::expiring(), NoLatency::raft());
    harness.fund(ALICE, FUNDING);
    // One at a time, so a slot pool deliberately smaller than this many holds still admits every one of
    // them: the scarcity this test wants is between a client and the sweep, not inside the setup.
    for _ in 0..HOLDS {
        let mut tx = harness.transfer(ALICE, BOB, AMOUNT);
        tx.flags = TransferFlags::PENDING;
        let ack = harness.run(tx);
        assert_eq!(ack.outcome, AckOutcome::Committed, "{ack:?}");
    }
    harness.tick_until_written(WRITTEN);
    harness.open_day(LIFETIME);
    harness
}

/// A client is served while a day's holds are being released. Expiry voids used to be acted on where the
/// notices are read — before intake, and with no bound at all — so with slots this scarce they took the
/// whole pool and this request came back `Overloaded`: a client refused for work nobody asked for.
#[test]
fn a_client_is_served_while_a_days_holds_are_expiring() {
    let mut harness = expiring_with(Capacity {
        slots: SLOTS,
        expiry_per_tick: 2,
        ..ReactorConfig::default().capacity
    });

    harness.tick_until("no void was ever admitted", |reactor| {
        reactor.metrics().holds_expired > 0
    });

    // Nothing about this transfer is special. That is the point: an ordinary request, submitted in the
    // middle of a mass expiry, has to come back.
    let tx = harness.transfer(ALICE, BOB, AMOUNT);
    let ack = harness.run(tx);
    assert_eq!(
        ack.outcome,
        AckOutcome::Committed,
        "a client was starved by the ledger's own expiry: {ack:?}\n  {:?}",
        harness.reactor.metrics()
    );

    // And the sweep was not stopped in exchange: it is still releasing while the client is being served,
    // which is the half a threshold would have given up.
    let expired = harness.reactor.metrics().holds_expired;
    harness.tick_until("the sweep stopped once a client arrived", |reactor| {
        reactor.metrics().holds_expired > expired
    });
    harness.assert_consistent();
}
