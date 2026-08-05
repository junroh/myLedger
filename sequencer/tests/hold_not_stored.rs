//! The one thing the pending engine says without being asked, and what the sequencer does about it.
//!
//! The engine's index is sized from a declared maximum and never grows, so an insert it cannot take is
//! a hold consensus committed that the store does not have. Its columns have already moved and its
//! client has already been told it committed; what can never happen now is the resolution that brings
//! that pending column back down. So this node's state has stopped following the log, and rule 19 says
//! detect and stop rather than detect and continue.

mod harness;

use ledger_base::{AckOutcome, LedgerError, TransferFlags};
use ledger_sequencer::LogKind;

use harness::*;

/// A hold the engine cannot store seals the apply path, and the seal is what a client sees next.
#[test]
fn a_committed_hold_the_engine_cannot_store_seals_the_apply_path() {
    let mut harness = Harness::with_stubs(NoLatency::tiny_index(), NoLatency::raft());
    harness.fund(ALICE, FUNDING);

    // More holds than the declared maximum, submitted without waiting: past the seal nothing is
    // answered by the apply path, so a test that waited for each ack would be waiting for the failure
    // it is testing for.
    for _ in 0..64 {
        let mut tx = harness.transfer(ALICE, BOB, 1);
        tx.flags = TransferFlags::PENDING;
        harness.submit(tx);
    }
    harness.tick_until("the engine never reported a hold it could not store", |r| {
        r.metrics().holds_not_stored > 0
    });

    assert!(
        harness.reactor.is_fail_stopped(),
        "the engine reported a hold it could not store and the node kept applying"
    );
    assert!(
        harness.logged(LogKind::HOLD_NOT_STORED),
        "the seal went unrecorded, so there is nothing to diagnose it with"
    );
    harness.assert_consistent();
}

/// Sealed is sealed: what is admitted afterwards is refused rather than judged, because judging it
/// would mean applying against bookkeeping that no longer follows the log. `FailStop` rather than
/// silence is what makes it a client's answer instead of a client's timeout.
#[test]
fn nothing_is_committed_after_the_seal() {
    let mut harness = Harness::with_stubs(NoLatency::tiny_index(), NoLatency::raft());
    harness.fund(ALICE, FUNDING);

    for _ in 0..64 {
        let mut tx = harness.transfer(ALICE, BOB, 1);
        tx.flags = TransferFlags::PENDING;
        harness.submit(tx);
    }
    harness.tick_until("the engine never reported a hold it could not store", |r| {
        r.metrics().holds_not_stored > 0
    });
    let committed_at_seal = harness.reactor.metrics().committed;

    // Whatever was already in flight is drained off, so the next ack is about the request below.
    for _ in 0..10_000 {
        harness.reactor.tick();
        while harness.poll().is_some() {}
    }

    let after = harness.transfer(ALICE, BOB, 1);
    let ack = harness.run(after);
    assert_eq!(
        ack.outcome,
        AckOutcome::Rejected(LedgerError::FailStop),
        "a request admitted after the seal was not refused"
    );
    assert_eq!(
        harness.reactor.metrics().committed,
        committed_at_seal,
        "the apply path committed something after it was sealed"
    );
    harness.assert_consistent();
}
