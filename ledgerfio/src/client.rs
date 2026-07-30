use ledger_base::{Ack, Consumer, Producer, Request, Transfer, TransferFlags};

use crate::clock::Clock;

/// Client side of the ledger: stamps submissions, marks batch boundaries, collects acks and
/// works out latency from the stamp that comes back.
pub struct Client {
    requests: Producer<Request>,
    acks: Consumer<Ack>,
    clock: Clock,
}

impl Client {
    pub fn new(requests: Producer<Request>, acks: Consumer<Ack>) -> Self {
        Self { requests, acks, clock: Clock::new() }
    }

    /// A standalone transfer. A linked leg cannot go this way: a chain has to arrive as one
    /// submission, which is what `submit_batch` does.
    pub fn submit(&self, tx: Transfer) -> Result<(), Transfer> {
        debug_assert!(
            !tx.flags.contains(TransferFlags::LINKED),
            "a linked leg needs submit_batch"
        );
        self.requests
            .push(Request::single(tx, self.clock.nanos()))
            .map_err(|request| request.tx)
    }

    /// One batch, one release store, one boundary. The boundary is what lets the sequencer
    /// reject a linked chain the client never terminated.
    pub fn submit_batch(&self, transfers: &[Transfer]) -> usize {
        let now = self.clock.nanos();
        let last = transfers.len().saturating_sub(1);
        self.requests.push_from(transfers.len(), |offset| Request {
            tx: transfers[offset],
            submitted_at_nanos: now,
            end_of_batch: offset == last,
        })
    }

    pub fn poll(&self) -> Option<Ack> {
        self.acks.pop()
    }

    pub fn now_nanos(&self) -> u64 {
        self.clock.nanos()
    }

    pub fn latency_nanos(&self, ack: &Ack) -> u64 {
        self.clock.nanos().saturating_sub(ack.submitted_at_nanos)
    }
}
