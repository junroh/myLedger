mod harness;

use std::time::Duration;

use harness::*;
use ledger_base::{AckOutcome, LedgerError, TransferFlags};
use ledger_stubkit::LatencyRange;
use ledger_raft::EchoRaftConfig;
use ledger_sequencer::LogKind;

/// Inside a chain a later leg may spend what an earlier leg brings in, which the speculative
/// overlay alone would refuse.
#[test]
fn a_later_leg_spends_what_an_earlier_leg_brings_in() {
    let mut harness = Harness::new();
    let incoming = harness.linked_leg(EXTERNAL, ALICE, FUNDING, false);
    let outgoing = harness.linked_leg(ALICE, BOB, FUNDING, true);

    let acks = harness.run_chain(&[incoming, outgoing]);

    assert!(acks.iter().all(|ack| ack.outcome == AckOutcome::Committed), "{acks:?}");
    assert_eq!(harness.columns(ALICE), (FUNDING, FUNDING, 0, 0));
    assert_eq!(harness.columns(BOB), (0, FUNDING, 0, 0));
    harness.assert_consistent();
}

/// One failing leg rejects the whole chain, including the legs that would have passed.
#[test]
fn one_failing_leg_rolls_back_the_whole_chain() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let before = harness.columns(ALICE);

    let good = harness.linked_leg(ALICE, BOB, 100, false);
    let too_big = harness.linked_leg(ALICE, BOB, FUNDING * 10, true);
    let acks = harness.run_chain(&[good, too_big]);

    assert!(acks.iter().all(|ack| matches!(ack.outcome, AckOutcome::Rejected(_))), "{acks:?}");
    assert_eq!(harness.columns(ALICE), before);
    assert_eq!(harness.columns(BOB), (0, 0, 0, 0));
    harness.assert_consistent();
}

/// A chain may create a hold and resolve it in the same submission. The engine does not know that
/// hold yet — it learns when the batch commits — but the chain is atomic, so a resolution cannot
/// outlive a creation that was refused. Afterwards the hold is the engine's like any other, and
/// its remainder must be what the chain left.
#[test]
fn a_chain_resolves_a_hold_it_created_itself() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);

    let mut hold = harness.transfer(ALICE, BOB, 300);
    hold.flags = TransferFlags::PENDING.with(TransferFlags::LINKED);
    let mut settle = harness.transfer(ALICE, BOB, 100);
    settle.pending_ref = hold.id;
    settle.flags = TransferFlags::POST_PENDING;

    let acks = harness.run_chain(&[hold, settle]);
    assert!(acks.iter().all(|ack| ack.outcome == AckOutcome::Committed), "{acks:?}");
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 200, 0));

    let too_much = harness.resolve(hold.id, ALICE, BOB, 300, TransferFlags::POST_PENDING);
    assert_eq!(
        too_much.outcome,
        AckOutcome::Rejected(LedgerError::SettleExceedsRemaining {
            remaining: 200,
            requested: 300
        }),
        "the chain took 100 of the hold"
    );

    let rest = harness.resolve(hold.id, ALICE, BOB, 200, TransferFlags::POST_PENDING);
    assert_eq!(rest.outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (300, FUNDING, 0, 0));
    harness.assert_consistent();
}

/// Outside the chain that created it, a hold does not exist until its batch commits. A resolution
/// from another submission is refused rather than judged against a hold that may never exist.
#[test]
fn a_hold_still_in_flight_cannot_be_resolved_by_another_submission() {
    let mut harness = Harness::with_stubs(
        NoLatency::pending(),
        EchoRaftConfig {
            round_trip: LatencyRange::fixed(Duration::from_millis(5)),
            ..NoLatency::raft()
        },
    );
    harness.fund(ALICE, FUNDING);

    let mut hold = harness.transfer(ALICE, BOB, 300);
    hold.flags = TransferFlags::PENDING;
    let mut settle = harness.transfer(ALICE, BOB, 300);
    settle.pending_ref = hold.id;
    settle.flags = TransferFlags::POST_PENDING;
    harness.submit(hold);
    harness.submit(settle);

    let acks = harness.drain_acks(2, "acks stalled");
    let outcome = |id| acks.iter().find(|ack| ack.tx_id == id).expect("ack").outcome;
    assert_eq!(outcome(hold.id), AckOutcome::Committed);
    assert!(
        matches!(outcome(settle.id), AckOutcome::Rejected(LedgerError::PendingRefNotFound(_))),
        "{acks:?}"
    );
    assert_eq!(harness.columns(ALICE), (0, FUNDING, 300, 0));
    harness.assert_consistent();
}

/// A chain whose terminating leg never arrives is rejected at the batch boundary, so it cannot gate
/// its lanes forever.
#[test]
fn an_unterminated_chain_is_rejected_at_the_batch_boundary() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);

    let orphan = harness.linked_leg(ALICE, BOB, 100, false);
    let acks = harness.run_chain(&[orphan]);
    assert_eq!(
        acks[0].outcome,
        AckOutcome::Rejected(LedgerError::LinkedChainUnterminated),
        "{acks:?}"
    );
    assert!(harness.logged(LogKind::CHAIN_ABORTED));

    let after = harness.transfer(ALICE, BOB, 100);
    assert_eq!(harness.run(after).outcome, AckOutcome::Committed, "lane still gated");
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 0, 0));
    harness.assert_consistent();
}
