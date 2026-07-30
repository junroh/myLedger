mod harness;

use std::time::Duration;

use ledger_base::{AckOutcome, LedgerError, TransferFlags};
use ledger_stubkit::LatencyRange;
use ledger_raft::EchoRaftConfig;
use ledger_pending::MemoryPendingConfig;
use ledger_sequencer::{ReactorConfig, SafetyPolicy};

use harness::*;

/// Contract 1: replies for one lane arrive in seq order. A stub that swaps two of them must be
/// caught as a gap, and the lane must be quarantined rather than judged out of order.
#[test]
fn an_out_of_order_reply_quarantines_the_lane() {
    let harness = with_swapped_replies(3);

    assert!(harness.reactor.metrics().seq_gaps > 0, "gap must be detected");
    assert_eq!(harness.reactor.quarantined(), &[ALICE]);
    assert!(!harness.reactor.is_fail_stopped(), "one lane is not the component");
}

/// Quarantine is recoverable: once the lane has drained an operator can release it, and the account
/// takes traffic again from the seq it last judged.
#[test]
fn a_drained_lane_can_be_released_and_used_again() {
    let mut harness = with_swapped_replies(3);
    harness.tick_until("lane never drained", |reactor| reactor.is_quiescent());

    harness.reactor.release_quarantine(ALICE).expect("drained lane");
    assert!(harness.reactor.quarantined().is_empty());

    let tx = harness.transfer(ALICE, BOB, 10);
    assert_eq!(harness.run(tx).outcome, AckOutcome::Committed);
    harness.assert_consistent();
}

/// Enough quarantined lanes means the component is broken, not one lane, so the sequencer stops
/// admitting anything. Clearing it is an operator action and the only way back.
#[test]
fn enough_quarantined_lanes_fail_stop_the_sequencer() {
    let mut harness = with_swapped_replies(1);
    assert!(harness.reactor.is_fail_stopped());

    let refused = harness.transfer(BOB, ALICE, 1);
    assert_eq!(harness.run(refused).outcome, AckOutcome::Rejected(LedgerError::FailStop));

    harness.tick_until("never drained", |reactor| reactor.is_quiescent());
    harness.reactor.clear_fail_stop().expect("nothing left in flight");
    harness.reactor.release_quarantine(ALICE).expect("drained lane");

    let tx = harness.transfer(ALICE, BOB, 10);
    assert_eq!(harness.run(tx).outcome, AckOutcome::Committed);
}

/// An unconstrained debit has no balance to protect, so it is not held behind an outstanding
/// pending reply on the same lane: it overtakes, and no fence is spent on it. A resolution stays
/// ordered, because its place decides which resolution of the hold wins.
#[test]
fn a_request_that_needs_no_balance_check_overtakes_the_pending_path() {
    let mut harness = Harness::with_stubs(
        MemoryPendingConfig {
            latency: LatencyRange::fixed(Duration::from_millis(5)),
            ..NoLatency::cold_pending()
        },
        NoLatency::raft(),
    );
    // EXTERNAL is the unconstrained account, and it is the debit side of both requests below.
    let (hold, _) = harness.hold(EXTERNAL, ALICE, 500);

    let mut settle = harness.transfer(EXTERNAL, ALICE, 100);
    settle.pending_ref = hold;
    settle.flags = TransferFlags::POST_PENDING;
    let overtaking = harness.transfer(EXTERNAL, BOB, 10);
    harness.submit(settle);
    harness.submit(overtaking);

    let acks = harness.drain_acks(2, "acks stalled");
    assert_eq!(acks[0].tx_id, overtaking.id, "the exempt request waited for the lookup");
    assert!(acks.iter().all(|ack| ack.outcome == AckOutcome::Committed), "{acks:?}");
    assert_eq!(harness.reactor.metrics().fences, 0);
    assert!(harness.reactor.metrics().order_exempt > 0);
    assert_eq!(harness.reactor.metrics().seq_gaps, 0);
}

/// Consensus still owes answers, and those answers must be applied in order. Clearing the fail-stop
/// before they land would apply them to a reactor that had already moved on, so it is refused until
/// the pipeline is empty.
#[test]
fn clearing_fail_stop_waits_for_consensus() {
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig {
            round_trip: LatencyRange::fixed(Duration::from_millis(20)),
            ..NoLatency::raft()
        },
    );
    let tx = harness.transfer(EXTERNAL, ALICE, 100);
    harness.submit(tx);
    harness.tick_until("nothing was proposed", |reactor| {
        reactor.backpressure().batches_in_flight > 0
    });

    assert_eq!(harness.reactor.clear_fail_stop(), Err(LedgerError::QuarantineDraining));

    harness.drain_acks(1, "the batch never committed");
    harness.tick_until("never drained", |reactor| reactor.is_quiescent());
    assert_eq!(harness.reactor.clear_fail_stop(), Ok(()));
    harness.assert_consistent();
}

/// Two settles of one lane dispatched together, which is what lets the stub return them swapped.
/// `fail_stop_after` is how many quarantined lanes the sequencer tolerates.
fn with_swapped_replies(fail_stop_after: usize) -> Harness {
    let mut harness = Harness::with_config(
        ReactorConfig {
            safety: SafetyPolicy { quarantine_fail_stop: fail_stop_after },
            ..ReactorConfig::default()
        },
        // Long enough that both replies are queued before either is delivered.
        MemoryPendingConfig {
            violate_order_every: 2,
            latency: LatencyRange::fixed(Duration::from_millis(5)),
            ..NoLatency::cold_pending()
        },
        NoLatency::raft(),
    );
    harness.fund(ALICE, FUNDING * 10);
    let (hold, _) = harness.hold(ALICE, BOB, 500);

    for _ in 0..2 {
        let mut settle = harness.transfer(ALICE, BOB, 10);
        settle.pending_ref = hold;
        settle.flags = TransferFlags::POST_PENDING;
        harness.submit(settle);
    }
    harness.drain_acks(2, "acks stalled");
    harness
}
