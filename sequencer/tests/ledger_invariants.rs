mod harness;

use std::time::Duration;

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort};
use ledger_base::{AckOutcome, TransferFlags};
use ledger_raft::EchoRaftConfig;
use ledger_stubkit::LatencyRange;

use harness::*;

/// Mixed traffic keeps double-entry intact: debits equal credits in both columns, no column goes
/// below zero, and no reservation survives once nothing is in flight.
#[test]
fn accounting_identities_hold_after_mixed_traffic() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING * 100);
    harness.fund(BOB, FUNDING * 100);
    mixed_traffic(&mut harness, 25);

    harness.assert_consistent();
}

/// The same traffic while consensus refuses every seventh batch. A rollback path that forgot a
/// column or a reservation shows up here and nowhere else.
#[test]
fn accounting_identities_hold_when_consensus_refuses_batches() {
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig {
            fail_every: 7,
            ..NoLatency::raft()
        },
    );
    fund_until_committed(&mut harness, ALICE, FUNDING * 100);
    fund_until_committed(&mut harness, BOB, FUNDING * 100);
    mixed_traffic(&mut harness, 25);

    assert!(
        harness.reactor.metrics().commit_failures > 0,
        "no batch was refused"
    );
    harness.assert_consistent();
}

/// A follower has none of the leader's handles, only the committed log and the account ids in it.
/// Replaying that log must reproduce the leader's balances exactly — including after rollbacks,
/// which must leave nothing in the log at all.
#[test]
fn the_committed_log_replays_to_the_same_balances_on_a_fresh_store() {
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig {
            keep_log: true,
            fail_every: 7,
            ..NoLatency::raft()
        },
    );
    fund_until_committed(&mut harness, ALICE, FUNDING * 10);
    fund_until_committed(&mut harness, BOB, FUNDING * 10);
    mixed_traffic(&mut harness, 10);

    let mut follower = MemoryAccounts::with_capacity(8);
    for account in ACCOUNTS {
        follower.open(account, LEDGER, AccountFlags::CONSTRAINED);
    }
    for effect in harness.raft_log() {
        follower.replay(&effect).expect("replay");
    }

    for account in ACCOUNTS {
        let handle = follower.resolve(account).expect("known account");
        let replayed = follower.record(handle);
        let leader = harness.record(account);
        assert_eq!(
            (
                leader.debits_posted(),
                leader.credits_posted(),
                leader.debits_pending(),
                leader.credits_pending()
            ),
            (
                replayed.debits_posted(),
                replayed.credits_posted(),
                replayed.debits_pending(),
                replayed.credits_pending()
            ),
            "account {account:?} diverged"
        );
    }
    assert_eq!(follower.totals(), harness.reactor.accounts().totals());
}

/// Everything a client can send: all four kinds, a partial settle followed by a void of the rest, a
/// linked chain, a re-submission, and a settle that asks for more than is left.
fn mixed_traffic(harness: &mut Harness, rounds: u32) {
    for round in 0..rounds {
        let post = harness.transfer(ALICE, BOB, 10);
        harness.run(post);
        harness.run(post); // the same transaction again: a duplicate, not a second transfer

        let (hold, _) = harness.hold(BOB, ALICE, 40);
        harness.resolve(hold, BOB, ALICE, 15, TransferFlags::POST_PENDING);
        harness.resolve(hold, BOB, ALICE, 100, TransferFlags::POST_PENDING); // over the remainder
        if round % 2 == 0 {
            harness.resolve(hold, BOB, ALICE, 0, TransferFlags::VOID_PENDING);
        }

        let incoming = harness.linked_leg(EXTERNAL, ALICE, 25, false);
        let outgoing = harness.linked_leg(ALICE, BOB, 25, true);
        harness.run_chain(&[incoming, outgoing]);
    }
}

/// Funding has to land before the traffic starts, and a refused batch only means retry.
fn fund_until_committed(harness: &mut Harness, account: ledger_base::AccountId, amount: i64) {
    loop {
        let transfer = harness.transfer(EXTERNAL, account, amount);
        if harness.run(transfer).outcome == AckOutcome::Committed {
            return;
        }
    }
}

/// Consensus answering for a batch that was not the one waiting cannot be paired with anything: the
/// effects belong to one batch and the slots to another. The sequencer must fail-stop and apply
/// nothing rather than ack the wrong requests, and whatever it applied before must still add up.
#[test]
fn a_commit_that_answers_the_wrong_batch_applies_nothing() {
    // A round trip long enough that every batch is still outstanding when the next is proposed,
    // which is what lets consensus answer them in the wrong order.
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig {
            reorder_every: 2,
            round_trip: LatencyRange::fixed(Duration::from_millis(50)),
            ..NoLatency::raft()
        },
    );
    fund_until_committed(&mut harness, ALICE, FUNDING * 100);

    // One request at a time, each proposed on its own, so several batches are in flight together.
    //
    // **The seal ends this loop, and it has to be allowed to.** Once the mispaired commit is noticed
    // nothing more is proposed, so a loop that insisted on six batches would wait for a seventh that can
    // never come — which is what it did, once in about a thousand concurrent runs: the reorder was
    // noticed at the fifth batch rather than after the sixth, and the count is a property of how the
    // fifty-millisecond round trips lined up rather than of anything this test is about.
    let batches = 6;
    for batch in 1..=batches {
        if harness.reactor.is_fail_stopped() {
            break;
        }
        let tx = harness.transfer(ALICE, BOB, 10);
        harness.submit(tx);
        harness.tick_until("a request was never proposed", |reactor| {
            reactor.metrics().proposed_batches > batch || reactor.is_fail_stopped()
        });
    }
    harness.tick_until("the reordering was never noticed", |reactor| {
        reactor.is_fail_stopped()
    });
    let applied = harness.reactor.accounts().totals();

    for _ in 0..1_000 {
        harness.reactor.tick();
    }
    assert_eq!(
        harness.reactor.accounts().totals(),
        applied,
        "a batch that could not be paired was applied anyway"
    );
    assert_eq!(
        applied.debits_posted, applied.credits_posted,
        "posted identity"
    );
    assert_eq!(
        applied.debits_pending, applied.credits_pending,
        "pending identity"
    );
}

/// An account component that loses count of what it applied, which is the cheapest way to make the
/// sequencer's own bookkeeping stop adding up. Everything else is the real store. Only the
/// release-only test below needs it, because a debug build turns a broken invariant into a panic.
#[cfg(not(debug_assertions))]
struct Forgetful(MemoryAccounts);

#[cfg(not(debug_assertions))]
impl AccountPort for Forgetful {
    fn resolve(&self, id: ledger_base::AccountId) -> Option<ledger_base::AcctHandle> {
        self.0.resolve(id)
    }

    fn record(&self, handle: ledger_base::AcctHandle) -> &ledger_base::ports::AccountRecord {
        self.0.record(handle)
    }

    fn apply(&mut self, effect: &ledger_base::Effect) -> Result<(), ledger_base::LedgerError> {
        self.0.apply(effect)
    }

    fn applied(&self) -> u64 {
        self.0.applied().saturating_sub(1)
    }

    fn totals(&self) -> ledger_base::ports::LedgerTotals {
        self.0.totals()
    }
}

/// The invariant check is itself a piece of safety machinery, so it has to be shown working: a
/// component whose count disagrees with the sequencer's seals the apply path rather than carrying on
/// with numbers that do not add up. Release only — deliberately breaking an invariant is what a
/// debug build turns into a panic, which is the other half of the behaviour being checked here.
#[cfg(not(debug_assertions))]
#[test]
fn bookkeeping_that_stops_adding_up_seals_the_apply_path() {
    let mut accounts = MemoryAccounts::with_capacity(4);
    for account in ACCOUNTS {
        accounts.open(account, LEDGER, AccountFlags::NONE);
    }
    let (requests, request_rx) = ledger_base::channel(64);
    let (ack_tx, acks) = ledger_base::channel(64);
    let (mut reactor, _log) = ledger_sequencer::Reactor::new(
        ledger_sequencer::ReactorConfig {
            batching: ledger_sequencer::BatchPolicy {
                size: 1,
                linger: std::time::Duration::ZERO,
                ..ledger_sequencer::ReactorConfig::default().batching
            },
            ..ledger_sequencer::ReactorConfig::default()
        },
        ledger_sequencer::Transport {
            requests: request_rx,
            acks: ack_tx,
        },
        Forgetful(accounts),
        ledger_pending::MemoryPending::start(NoLatency::pending()).expect("a test engine config"),
        ledger_idempotency::MemoryIdem::start(NoLatency::idem()),
        ledger_raft::EchoRaft::start(NoLatency::raft()),
    )
    .expect("config");

    let transfer = ledger_base::Transfer {
        id: ledger_base::TxId(1),
        pending_ref: ledger_base::TxId::ABSENT,
        debit_account: EXTERNAL,
        credit_account: ALICE,
        amount: 100,
        ledger: LEDGER,
        flags: ledger_base::TransferFlags::NONE,
    };
    requests
        .push(ledger_base::Request::single(transfer, 0))
        .expect("client queue");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while reactor.metrics().invariant_breaks == 0 {
        reactor.tick();
        assert!(
            std::time::Instant::now() < deadline,
            "the disagreement was never noticed"
        );
    }
    assert!(
        reactor.is_fail_stopped(),
        "a broken invariant must stop the node"
    );
    assert_eq!(
        reactor.audit(),
        Err(ledger_sequencer::Broken::AccountViewDisagrees)
    );
    let _ = acks.pop();
}
