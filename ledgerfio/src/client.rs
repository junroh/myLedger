use ledger_base::{Ack, Consumer, Producer, Request, Transfer, TransferFlags};
use ledger_sequencer::{PauseCause, PressureView};

use crate::clock::Clock;

/// A submission the ledger would not take, and why.
///
/// **The transfer comes back and so does the reason.** A full queue is the only thing the client can see
/// for itself, and it is the same symptom whatever caused it — a slow disk, a slow consensus, or this
/// client not collecting its own acks. The three want different reactions, so the answer carries which.
#[derive(Debug, Clone, Copy)]
pub struct Refused {
    pub tx: Transfer,
    /// The last backlog to stop intake — see `PressureView`, which explains why the last one and not the
    /// current one is the answer worth having.
    pub cause: PauseCause,
    /// Whether intake is stopped as this refusal is handed back.
    pub paused_now: bool,
}

/// Client side of the ledger: stamps submissions, marks batch boundaries, collects acks and
/// works out latency from the stamp that comes back.
pub struct Client {
    requests: Producer<Request>,
    acks: Consumer<Ack>,
    pressure: PressureView,
    clock: Clock,
}

impl Client {
    pub fn new(requests: Producer<Request>, acks: Consumer<Ack>, pressure: PressureView) -> Self {
        Self {
            requests,
            acks,
            pressure,
            clock: Clock::new(),
        }
    }

    /// What is holding the ledger back, for anything rate-limiting in front of a client and for a batch
    /// submission, which is refused by count rather than by value. `PauseCause::None` is a sequencer
    /// nothing has ever held back, so a refusal beside it is a client outrunning one that is keeping up.
    pub fn pressure(&self) -> &PressureView {
        &self.pressure
    }

    /// A standalone transfer. A linked leg cannot go this way: a chain has to arrive as one
    /// submission, which is what `submit_batch` does.
    pub fn submit(&self, tx: Transfer) -> Result<(), Refused> {
        debug_assert!(
            !tx.flags.contains(TransferFlags::LINKED),
            "a linked leg needs submit_batch"
        );
        self.requests
            .push(Request::single(tx, self.clock.nanos()))
            .map_err(|request| Refused {
                tx: request.tx,
                cause: self.pressure.cause(),
                paused_now: self.pressure.paused_now(),
            })
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
