mod harness;

use harness::*;
use ledger_base::{AckOutcome, LedgerError, TransferFlags};

/// The four transfer kinds each move the four columns the design specifies, on both sides.
#[test]
fn every_kind_moves_the_columns_the_design_specifies() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);

    let tx = harness.transfer(ALICE, BOB, 100);
    assert_eq!(harness.run(tx).outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 0, 0));
    assert_eq!(harness.columns(BOB), (0, 100, 0, 0));

    let (hold, ack) = harness.hold(ALICE, BOB, 300);
    assert_eq!(ack.outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 300, 0));
    assert_eq!(harness.columns(BOB), (0, 100, 0, 300));
    assert_eq!(harness.available(ALICE), FUNDING - 100 - 300);

    let ack = harness.resolve(hold, ALICE, BOB, 100, TransferFlags::POST_PENDING);
    assert_eq!(ack.outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (200, FUNDING, 200, 0));
    assert_eq!(harness.columns(BOB), (0, 200, 0, 200));

    let ack = harness.resolve(hold, ALICE, BOB, 0, TransferFlags::VOID_PENDING);
    assert_eq!(ack.outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (200, FUNDING, 0, 0));
    assert_eq!(harness.columns(BOB), (0, 200, 0, 0));
    assert_eq!(harness.available(ALICE), FUNDING - 200);
    harness.assert_consistent();
}

/// A hold may be settled repeatedly, but never for more than it has left.
#[test]
fn a_settle_larger_than_the_remaining_hold_is_rejected() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let (hold, _) = harness.hold(ALICE, BOB, 300);
    harness.resolve(hold, ALICE, BOB, 250, TransferFlags::POST_PENDING);

    let ack = harness.resolve(hold, ALICE, BOB, 100, TransferFlags::POST_PENDING);
    assert_eq!(
        ack.outcome,
        AckOutcome::Rejected(LedgerError::SettleExceedsRemaining { remaining: 50, requested: 100 })
    );
    assert_eq!(harness.columns(ALICE), (250, FUNDING, 50, 0));
    harness.assert_consistent();
}

/// A rejected debit changes nothing, and leaves no reservation behind.
#[test]
fn a_debit_beyond_available_balance_is_rejected_without_changing_state() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let before = harness.columns(ALICE);

    let tx = harness.transfer(ALICE, BOB, FUNDING + 1);
    let ack = harness.run(tx);
    assert_eq!(
        ack.outcome,
        AckOutcome::Rejected(LedgerError::InsufficientBalance {
            available: FUNDING,
            requested: FUNDING + 1
        })
    );
    assert_eq!(harness.columns(ALICE), before);
    harness.assert_consistent();
}

/// Money on its way in is credited pending, and pending credit is not availability until it
/// settles.
#[test]
fn an_incoming_hold_is_not_spendable_until_it_settles() {
    let mut harness = Harness::new();
    let (incoming, ack) = harness.hold(EXTERNAL, ALICE, 500);
    assert_eq!(ack.outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (0, 0, 0, 500));
    assert_eq!(harness.available(ALICE), 0);

    let spend = harness.transfer(ALICE, BOB, 200);
    assert!(matches!(
        harness.run(spend).outcome,
        AckOutcome::Rejected(LedgerError::InsufficientBalance { .. })
    ));

    let ack = harness.resolve(incoming, EXTERNAL, ALICE, 300, TransferFlags::POST_PENDING);
    assert_eq!(ack.outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (0, 300, 0, 200));
    assert_eq!(harness.available(ALICE), 300);
    harness.assert_consistent();
}
