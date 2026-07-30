mod harness;

use harness::*;
use ledger_base::{AckOutcome, LedgerError};

/// Contract 3: a transaction id is answered once. Re-submitting the same transfer is a duplicate,
/// not a second transfer, and the money moves once. A different body under the same id is a client
/// bug and is refused rather than treated as either.
#[test]
fn a_resubmitted_transaction_moves_the_money_once() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let transfer = harness.transfer(ALICE, BOB, 100);

    assert_eq!(harness.run(transfer).outcome, AckOutcome::Committed);
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 0, 0));

    assert_eq!(harness.run(transfer).outcome, AckOutcome::Duplicate, "same body");
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 0, 0), "a duplicate moves nothing");

    let mut altered = transfer;
    altered.amount = 500;
    assert_eq!(
        harness.run(altered).outcome,
        AckOutcome::Rejected(LedgerError::DuplicateDifferentBody),
        "a different body under a used id"
    );
    assert_eq!(harness.columns(ALICE), (100, FUNDING, 0, 0));
    assert_eq!(harness.reactor.metrics().duplicates, 1);
    harness.assert_consistent();
}
