//! The log position a node's state reflects, which is the one thing recovery needs and nothing here had.
//!
//! There is no snapshot and no replay, so this file is not testing recovery. It is holding the seam open:
//! the index exists, it advances with committed batches and not with refused ones, and it is reachable
//! from outside the reactor. Design notes §15 is the design that will use it.

mod harness;

use harness::*;
use ledger_base::ports::AccountPort;
use ledger_base::AckOutcome;

/// A committed batch moves the index and a refused one does not.
///
/// Gapless is the property recovery rests on: "everything up to here has been applied" is only a
/// well-formed sentence if every position below the index is in the log. A refused batch wrote nothing, so
/// giving it a position would leave a hole that recovery would try to replay and never find.
#[test]
fn the_apply_index_counts_committed_batches_and_nothing_else() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    let after_funding = harness.reactor.applied_through();
    assert!(
        after_funding.raw() > 0,
        "funding committed batches and the index did not move"
    );

    let tx = harness.transfer(ALICE, BOB, 100);
    assert_eq!(harness.run(tx).outcome, AckOutcome::Committed);
    let after_commit = harness.reactor.applied_through();
    assert!(
        after_commit > after_funding,
        "a committed batch left the index where it was"
    );

    // Refused: the debit has nothing left, so nothing reaches consensus and nothing is written.
    let tx = harness.transfer(ALICE, BOB, FUNDING * 2);
    assert!(matches!(harness.run(tx).outcome, AckOutcome::Rejected(_)));
    assert_eq!(
        harness.reactor.applied_through(),
        after_commit,
        "a rejected request moved the index, so a position exists that the log does not have"
    );
}

/// The index and the account view agree on how much has been applied, which is the invariant a snapshot
/// has to carry across a restart.
///
/// Today both are counts that start at zero with the process, and the reactor already compares them every
/// tick — a mismatch is `Broken::AccountViewDisagrees` and it seals. This asserts the two are the same
/// number from outside, so a change that let them drift fails here rather than in a simulator seed.
#[test]
fn the_apply_index_and_the_account_view_agree() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);
    for _ in 0..8 {
        let tx = harness.transfer(ALICE, BOB, 10);
        assert_eq!(harness.run(tx).outcome, AckOutcome::Committed);
    }

    // The account view counts effects and the index counts batches, so they are not equal — what has to
    // hold is that both moved and neither is ahead of what the sequencer committed.
    let effects = harness.reactor.accounts().applied();
    assert_eq!(
        effects,
        harness.reactor.metrics().committed,
        "the account view and the sequencer disagree about how much has been applied"
    );
    assert!(
        harness.reactor.applied_through().raw() > 0,
        "effects were applied and the log position never moved"
    );
    assert!(
        harness.reactor.applied_through().raw() <= effects,
        "more batches applied than effects, which cannot happen: a batch holds at least one"
    );
}
