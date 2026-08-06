//! Retention, from the client's side: a hold nobody resolves does not hold its money for ever.
//!
//! The engine notices that a hold's record has outlived its retention and proposes releasing what is left
//! of it. The sequencer judges that like any other resolution — it has to, because a settle the client
//! submitted may be in flight for the same hold and only the judge sees both — and the money comes back
//! through the ordinary apply path.

mod harness;

use ledger_base::{AckOutcome, Amount, LedgerError, TransferFlags, TxId};
use ledger_pending::RECORDS_PER_BLOCK;

use harness::*;

/// A record belongs to a day — a segment — only once the writeback buffer has compacted it out, so these
/// tests write more than a block's worth. What is left in the open block has no segment yet and is
/// therefore not expired by anything, which is correct: an unwritten record is not one retention has
/// reached.
const HOLDS: usize = RECORDS_PER_BLOCK + 1;
const AMOUNT: Amount = 5;

/// Two days: one of promised retention, one of the grace that keeps deletion from being early.
const LIFETIME: u64 = 2;

/// Holds still in the open block, and holds whose record reached a segment.
const BUFFERED: usize = HOLDS % RECORDS_PER_BLOCK;
const WRITTEN: usize = HOLDS - BUFFERED;

fn holds_written_on_day_zero() -> Harness {
    let mut harness = Harness::with_stubs(NoLatency::expiring(), NoLatency::raft());
    harness.fund(ALICE, FUNDING);
    for _ in 0..HOLDS {
        let mut tx = harness.transfer(ALICE, BOB, AMOUNT);
        tx.flags = TransferFlags::PENDING;
        harness.submit(tx);
    }
    harness.drain_acks(HOLDS, "the holds were never committed");
    // The holds are committed; the records are written a moment later, on the engine's thread. The day may
    // not move until they are, or they belong to the next day — see `tick_until_written`.
    harness.tick_until_written(WRITTEN as u64);
    harness
}

/// The whole point of expiry: holds nobody resolves are released, and the money is spendable again.
#[test]
fn holds_that_outlive_their_retention_are_released_without_a_client_asking() {
    let mut harness = holds_written_on_day_zero();
    assert_eq!(
        harness.available(ALICE),
        FUNDING - HOLDS as Amount * AMOUNT,
        "the holds reserved nothing"
    );

    // Nothing before the promise and its grace have both passed. This is the edge that matters: deleting
    // early would refuse a resolution still entitled to arrive, which is a wrong answer rather than a cost.
    harness.open_day(LIFETIME - 1);
    for _ in 0..5_000 {
        harness.reactor.tick();
    }
    assert_eq!(
        harness.reactor.metrics().holds_expired,
        0,
        "a hold was released before its lifetime ran out"
    );

    harness.open_day(LIFETIME);
    let left_reserved = BUFFERED as Amount * AMOUNT;
    harness.tick_until("the expired holds were never released", |reactor| {
        Harness::pending_column(reactor) == left_reserved
    });

    assert_eq!(
        harness.available(ALICE),
        FUNDING - left_reserved,
        "the released holds did not come back to the balance"
    );
    // Every written hold had a void admitted for it, and more besides: the sweep goes round again until a
    // pass finds nothing, so a void whose commit has not landed yet is offered a second time. That is
    // harmless — its id is derived from the hold, so the judge refuses it against a hold already taken —
    // and the column above is what proves each hold was released exactly once. The churn is bounded by the
    // voids allowed per round, and how much of it there is depends on how long a pass takes: this index is
    // a thousand slots, where a deployment's is tens of millions and a pass takes far longer than a commit.
    let metrics = harness.reactor.metrics();
    assert!(
        metrics.holds_expired as usize >= WRITTEN,
        "not every written hold had a void admitted: {metrics:?}"
    );
    harness.assert_consistent();
}

/// The ledger does not answer itself. Nobody submitted these voids, so an ack for one would put a
/// transaction id no client sent into the client's stream — which a client would have to either ignore or
/// mistake for its own.
#[test]
fn the_client_is_not_acked_for_voids_it_never_sent() {
    let mut harness = holds_written_on_day_zero();

    harness.open_day(LIFETIME);
    let left_reserved = BUFFERED as Amount * AMOUNT;
    harness.tick_until("the expired holds were never released", |reactor| {
        Harness::pending_column(reactor) == left_reserved
    });

    assert!(
        harness.poll().is_none(),
        "the client was acked for the ledger's own void"
    );
    harness.assert_consistent();
}

/// The id of an expiry void is derived from the hold it resolves, so two leaders propose the same one and
/// the second is a duplicate rather than a second void. That reservation is what makes the top bit
/// off-limits to clients: one using it could collide with a derived id, and idempotency would answer a real
/// transfer as a duplicate.
#[test]
fn a_client_may_not_use_the_ids_reserved_for_the_ledgers_own_resolutions() {
    let mut harness = Harness::new();
    harness.fund(ALICE, FUNDING);

    let mut tx = harness.transfer(ALICE, BOB, 10);
    tx.id = TxId::expiry_void_of(tx.id);
    assert_eq!(
        harness.run(tx).outcome,
        AckOutcome::Rejected(LedgerError::ReservedTransactionId)
    );
    harness.assert_consistent();
}

/// A hold the client resolved before its retention ran out must not be resolved a second time. The engine
/// still offers it — the sweep works from an index it walks over many rounds, and a settle it has not seen
/// yet is exactly the case — and the judge is what refuses it. That is why an expiry void is judged rather
/// than applied.
#[test]
fn a_hold_the_client_resolved_is_never_resolved_twice() {
    let mut harness = holds_written_on_day_zero();
    // Funding took the first id, so this is the first hold, and its record is in the first block written.
    let hold = TxId(2);
    let settled = harness.resolve(hold, ALICE, BOB, AMOUNT, TransferFlags::POST_PENDING);
    assert_eq!(settled.outcome, AckOutcome::Committed, "{settled:?}");

    harness.open_day(LIFETIME);
    let left_reserved = BUFFERED as Amount * AMOUNT;
    harness.tick_until("the expired holds were never released", |reactor| {
        Harness::pending_column(reactor) == left_reserved
    });

    // Every hold moved exactly its own amount, once: the settled one to posted, the written rest released,
    // and the unwritten rest still reserved.
    let (debits, _, debits_pending, _) = harness.columns(ALICE);
    assert_eq!(debits, AMOUNT, "the hold was resolved twice");
    assert_eq!(debits_pending, left_reserved);
    assert_eq!(harness.available(ALICE), FUNDING - AMOUNT - left_reserved);
    // No void was even offered for it: the settle removed its index entry, and the sweep works from the
    // index. The case where a settle is still *in flight* when a void is offered is the one the judge has to
    // refuse, and that is a race a fixed harness cannot arrange — `ledgersim` reports it as
    // `expiry refused`, in the thousands.
    harness.assert_consistent();
}
