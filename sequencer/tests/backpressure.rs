mod harness;

use std::time::Duration;

use ledger_base::{AckOutcome, LedgerError};
use ledger_raft::EchoRaftConfig;
use ledger_sequencer::{Capacity, PauseCause, ReactorConfig};
use ledger_stubkit::LatencyRange;

use harness::*;

/// Every internal queue is bounded, so reaching one refuses work instead of growing. With four work
/// slots and consensus holding them all, the next request is refused as overloaded — and the lane it
/// would have used is still usable once the slots come back.
#[test]
fn a_full_slot_pool_refuses_new_requests() {
    let slots = 4;
    let mut harness = Harness::with_config(
        ReactorConfig {
            capacity: Capacity {
                slots,
                ..ReactorConfig::default().capacity
            },
            ..ReactorConfig::default()
        },
        NoLatency::pending(),
        EchoRaftConfig {
            round_trip: LatencyRange::fixed(Duration::from_millis(20)),
            ..NoLatency::raft()
        },
    );

    let requests = slots * 4;
    for _ in 0..requests {
        let tx = harness.transfer(EXTERNAL, ALICE, 10);
        harness.submit(tx);
    }
    let acks = harness.drain_acks(requests, "acks stalled");

    let refused = acks
        .iter()
        .filter(|ack| ack.outcome == AckOutcome::Rejected(LedgerError::Overloaded))
        .count();
    assert!(refused > 0, "the slot pool never filled: {acks:?}");
    assert_eq!(harness.reactor.metrics().slot_exhaustion as usize, refused);

    let after = harness.transfer(EXTERNAL, ALICE, 10);
    assert_eq!(
        harness.run(after).outcome,
        AckOutcome::Committed,
        "slots never came back"
    );
    harness.assert_consistent();
}

/// A client that stops reading its acks must become backpressure, not unbounded memory: the ack
/// backlog fills, intake stops admitting, and everything is answered once the client resumes.
///
/// **And the pause says which backlog it was**, which is the whole of what a refused client can learn. A
/// full request queue is the same symptom whatever caused it — a slow store, a slow consensus, or this
/// client not collecting its own acks — and the three want different reactions. Here it is the third, and
/// the published cause has to say so rather than say "full".
#[test]
fn a_client_that_stops_reading_pauses_intake() {
    let queue = 8;
    let mut harness = Harness::with_client_queue(
        ReactorConfig {
            capacity: Capacity {
                ack_backlog: 2,
                ..ReactorConfig::default().capacity
            },
            ..ReactorConfig::default()
        },
        queue,
    );

    // Two waves without polling: the first fills the ack queue, the second has nowhere to go.
    for _ in 0..queue {
        let tx = harness.transfer(EXTERNAL, ALICE, 10);
        harness.submit(tx);
    }
    harness.tick_until("first wave stalled", |reactor| {
        reactor.metrics().committed as usize == queue
    });
    for _ in 0..queue {
        let tx = harness.transfer(EXTERNAL, ALICE, 10);
        harness.submit(tx);
    }
    harness.tick_until("the ack backlog never filled", |reactor| {
        reactor.metrics().intake_pauses > 0
    });
    assert_eq!(
        harness.reactor.pressure().cause(),
        PauseCause::AcksUnread,
        "intake paused without saying which backlog stopped it"
    );

    let acks = harness.drain_acks(queue * 2, "acks stalled once the client resumed");
    assert!(
        acks.iter().all(|ack| ack.outcome == AckOutcome::Committed),
        "{acks:?}"
    );
    harness.assert_consistent();
}
