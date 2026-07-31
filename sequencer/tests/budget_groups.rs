mod harness;

use harness::*;
use ledger_base::{AckOutcome, LedgerError, TransferFlags};

/// A chain may resolve a hold it created itself, but not one that joined a budget group: the group's
/// membership and remainder are the engine's to report, and the engine only learns of the hold when
/// the batch commits. Refusing the chain is the safe answer — judging the group on invented
/// membership would let one member move alone.
#[test]
fn a_chain_cannot_resolve_a_budget_group_hold_it_created() {
    let mut harness = Harness::new();
    harness.fund(POINTS, 100);

    let mut hold = harness.transfer(POINTS, BOB, 100);
    hold.flags = TransferFlags::PENDING.with(TransferFlags::LINKED);
    hold.pending_ref = hold.id; // the client names the group after its first hold
    let mut settle = harness.transfer(POINTS, BOB, 100);
    settle.pending_ref = hold.id;
    settle.flags = TransferFlags::POST_PENDING;

    let acks = harness.run_chain(&[hold, settle]);
    assert!(
        acks.iter()
            .all(|ack| matches!(ack.outcome, AckOutcome::Rejected(_))),
        "{acks:?}"
    );
    assert_eq!(harness.columns(POINTS), (0, 100, 0, 0), "nothing moved");
    harness.assert_consistent();
}

/// Scenario 1: one payment drawn from three balances. The holds share a budget, so any resolution
/// must move all of them, in full, in one chain.
#[test]
fn a_shared_budget_group_resolves_as_one_unit() {
    let mut harness = Harness::new();
    harness.fund(POINTS, 30);
    harness.fund(DEPOSIT, 70);
    harness.fund(ALICE, 50);

    let mut legs = [
        harness.linked_leg(POINTS, BOB, 30, false),
        harness.linked_leg(DEPOSIT, BOB, 70, false),
        harness.linked_leg(ALICE, BOB, 50, true),
    ];
    // The client names the group, conventionally after its first hold.
    let budget = legs[0].id;
    for leg in legs.iter_mut() {
        leg.flags = leg.flags.with(TransferFlags::PENDING);
        leg.pending_ref = budget;
    }
    let holds = [legs[0].id, legs[1].id, legs[2].id];

    let acks = harness.run_chain(&legs);
    assert!(
        acks.iter().all(|ack| ack.outcome == AckOutcome::Committed),
        "{acks:?}"
    );
    assert_eq!(harness.columns(POINTS), (0, 30, 30, 0));

    let alone = harness.resolve(holds[0], POINTS, BOB, 30, TransferFlags::POST_PENDING);
    assert_eq!(
        alone.outcome,
        AckOutcome::Rejected(LedgerError::SharedBudgetGroupRequired),
        "one hold cannot cover a group of three"
    );

    let short =
        harness.resolve_together(&[(holds[0], POINTS, BOB, 30), (holds[1], DEPOSIT, BOB, 70)]);
    assert!(
        short.iter().all(
            |ack| ack.outcome == AckOutcome::Rejected(LedgerError::SharedBudgetGroupIncomplete)
        ),
        "a chain that leaves a member out: {short:?}"
    );

    let half = harness.resolve_together(&[
        (holds[0], POINTS, BOB, 10),
        (holds[1], DEPOSIT, BOB, 70),
        (holds[2], ALICE, BOB, 50),
    ]);
    assert!(
        half.iter().all(
            |ack| ack.outcome == AckOutcome::Rejected(LedgerError::PartialResolutionNotAllowed)
        ),
        "a chain that moves part of a member: {half:?}"
    );

    let acks = harness.resolve_together(&[
        (holds[0], POINTS, BOB, 30),
        (holds[1], DEPOSIT, BOB, 70),
        (holds[2], ALICE, BOB, 50),
    ]);
    assert!(
        acks.iter().all(|ack| ack.outcome == AckOutcome::Committed),
        "{acks:?}"
    );
    assert_eq!(harness.columns(POINTS), (30, 30, 0, 0));
    assert_eq!(harness.columns(DEPOSIT), (70, 70, 0, 0));
    assert_eq!(harness.columns(BOB), (0, 150, 0, 0));
    harness.assert_consistent();
}
