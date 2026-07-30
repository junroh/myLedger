mod harness;

use std::time::Duration;

use ledger_base::{AckOutcome, LedgerError, ManualClock, TransferFlags};
use ledger_stubkit::LatencyRange;
use ledger_raft::EchoRaftConfig;
use ledger_sequencer::{BatchPolicy, ReactorConfig};

use harness::*;

/// Path separation: judging must not wait behind consensus. Everything submitted is judged while
/// the first proposal is still in flight.
#[test]
fn a_slow_consensus_path_does_not_delay_judging() {
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig {
            round_trip: LatencyRange::fixed(Duration::from_millis(200)),
            ..EchoRaftConfig::default()
        },
    );
    let requests = 32;
    let fund = harness.transfer(EXTERNAL, ALICE, FUNDING * 100);
    harness.submit(fund);
    for _ in 1..requests {
        let tx = harness.transfer(EXTERNAL, BOB, 1);
        harness.submit(tx);
    }

    harness.tick_until("judging stalled behind consensus", |reactor| {
        reactor.metrics().judged >= requests
    });
    assert_eq!(harness.reactor.metrics().committed, 0, "consensus was still in flight");
}

/// A commit that consensus refuses rejects the request and leaves the ledger exactly as it was.
#[test]
fn a_failed_commit_releases_the_overlay_and_leaves_balances_untouched() {
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig { fail_every: 1, ..NoLatency::raft() },
    );
    let tx = harness.transfer(EXTERNAL, ALICE, FUNDING);

    assert_eq!(harness.run(tx).outcome, AckOutcome::Rejected(LedgerError::RaftCommitFailed));
    assert_eq!(harness.columns(ALICE), (0, 0, 0, 0));
    harness.assert_consistent();
}

/// A settle whose batch consensus refuses must give the hold's remainder back. Otherwise the money
/// is stranded: no balance moved and nothing can resolve the hold either.
#[test]
fn a_failed_settle_gives_the_hold_remainder_back() {
    // The stub fails the third proposal, and one effect per batch makes that the settle.
    let raft = EchoRaftConfig { fail_every: 3, ..NoLatency::raft() };
    let mut harness = Harness::with_stubs(NoLatency::pending(), raft);
    harness.fund(ALICE, FUNDING);
    let (hold, ack) = harness.hold(ALICE, BOB, 300);
    assert_eq!(ack.outcome, AckOutcome::Committed);

    let failed = harness.resolve(hold, ALICE, BOB, 100, TransferFlags::POST_PENDING);
    assert_eq!(failed.outcome, AckOutcome::Rejected(LedgerError::RaftCommitFailed));
    assert_eq!(harness.columns(ALICE), (0, FUNDING, 300, 0), "nothing moved");

    let whole = harness.resolve(hold, ALICE, BOB, 300, TransferFlags::POST_PENDING);
    assert_eq!(whole.outcome, AckOutcome::Committed, "the remainder was not given back");
    assert_eq!(harness.columns(ALICE), (300, FUNDING, 0, 0));
    harness.assert_consistent();
}

/// A hold the engine has just been told to create is already in its overlay, so resolving it costs
/// no lookup. These two counters are the hit ratio a run reports, so they must not drift.
#[test]
fn a_created_hold_is_resolved_without_a_lookup() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let (hold, _) = harness.hold(ALICE, BOB, 300);

    harness.resolve(hold, ALICE, BOB, 100, TransferFlags::POST_PENDING);
    harness.resolve(hold, ALICE, BOB, 100, TransferFlags::POST_PENDING);

    assert_eq!(harness.reactor.metrics().pending_lookups, 0);
    assert_eq!(harness.reactor.metrics().pending_hits, 2);
    assert_eq!(harness.reactor.metrics().fences, 0, "nothing waited on the pending path");
}

/// Once the overlay has dropped the hold, the same resolution has to fetch it — and then the
/// fence applies again, because the lane is waiting on an external reply.
#[test]
fn a_hold_the_overlay_dropped_is_fetched_again() {
    let mut harness = Harness::with_stubs(NoLatency::cold_pending(), NoLatency::raft());
    harness.fund(ALICE, FUNDING);
    let (hold, _) = harness.hold(ALICE, BOB, 300);

    let ack = harness.resolve(hold, ALICE, BOB, 100, TransferFlags::POST_PENDING);
    assert_eq!(ack.outcome, AckOutcome::Committed);
    assert_eq!(harness.reactor.metrics().pending_lookups, 1);
    assert_eq!(harness.reactor.metrics().pending_hits, 0);
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 200, 0));
    harness.assert_consistent();
}

/// Eviction may not drop a hold that dispatched requests are still going to read. With a policy
/// that evicts everything it can on every round, a run of resolutions of one hold must still all
/// commit: whichever of them is in flight keeps the hold in the overlay.
#[test]
fn resolutions_in_flight_keep_their_hold_in_the_overlay() {
    let mut harness = Harness::with_stubs(NoLatency::cold_pending(), NoLatency::raft());
    harness.fund(ALICE, FUNDING);
    let (hold, _) = harness.hold(ALICE, BOB, 300);

    let settles = 16;
    for _ in 0..settles {
        let mut settle = harness.transfer(ALICE, BOB, 10);
        settle.pending_ref = hold;
        settle.flags = TransferFlags::POST_PENDING;
        harness.submit(settle);
    }
    let acks = harness.drain_acks(settles, "acks stalled");

    assert!(acks.iter().all(|ack| ack.outcome == AckOutcome::Committed), "{acks:?}");
    assert_eq!(harness.columns(ALICE), (160, FUNDING, 140, 0));
    harness.assert_consistent();
}

/// A hold the engine says is not there is refused, and asking again would get the same answer, so
/// the answer is kept: a second resolution of the same missing hold costs no second lookup.
#[test]
fn an_answer_of_not_there_is_not_asked_twice() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let missing = harness.transfer(ALICE, BOB, 10).id;

    for _ in 0..2 {
        let mut settle = harness.transfer(ALICE, BOB, 10);
        settle.pending_ref = missing;
        settle.flags = TransferFlags::POST_PENDING;
        assert_eq!(
            harness.run(settle).outcome,
            AckOutcome::Rejected(LedgerError::PendingRefNotFound(missing))
        );
    }
    assert_eq!(harness.reactor.metrics().pending_lookups, 1, "the second asked again");
    assert_eq!(harness.reactor.metrics().pending_hits, 0, "a hold that is not there is not a hit");
    harness.assert_consistent();
}

/// A batch that is not full waits for its linger and is proposed as soon as it expires. The clock
/// is injected, so the decision is reproducible rather than timed.
#[test]
fn a_partial_batch_waits_for_its_linger_and_no_longer() {
    let clock = ManualClock::new(0);
    let linger = Duration::from_micros(200);
    let mut harness = Harness::with_clock(
        BatchPolicy { size: 1_000, linger, ..ReactorConfig::default().batching },
        clock.clone(),
    );

    let tx = harness.transfer(EXTERNAL, ALICE, 100);
    harness.submit(tx);
    harness.tick_until("judging stalled", |reactor| reactor.metrics().judged == 1);
    for _ in 0..100 {
        harness.reactor.tick();
    }
    assert_eq!(harness.reactor.metrics().proposed_batches, 0, "linger had not expired");

    clock.advance(linger.as_nanos() as u64);
    harness.tick_until("batch never proposed", |reactor| reactor.metrics().proposed_batches == 1);
    assert_eq!(harness.drain_acks(1, "no ack after commit")[0].outcome, AckOutcome::Committed);
}
