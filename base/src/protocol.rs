use crate::error::LedgerError;
use crate::ids::{AccountId, Seq, TxId};
use crate::transfer::Transfer;

/// What a client submits. The submit stamp is the client's own clock reading; the ledger
/// only carries it back so the client can measure its own latency.
///
/// `end_of_batch` marks the last request of a submission. A batch is not a unit of
/// consistency — it is decomposed on intake — but it is the boundary a linked chain must
/// close within, which is what keeps an abandoned chain from blocking its lanes forever.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub tx: Transfer,
    pub submitted_at_nanos: u64,
    pub end_of_batch: bool,
}

impl Request {
    pub const fn single(tx: Transfer, submitted_at_nanos: u64) -> Self {
        Self { tx, submitted_at_nanos, end_of_batch: true }
    }
}

crate::layout_claim!(REQUEST_LAYOUT: Request, size = 80, crate::layout::LineFit::Straddles(crate::layout::STREAMED));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    Committed,
    Duplicate,
    Rejected(LedgerError),
}

#[derive(Debug, Clone, Copy)]
pub struct Ack {
    pub tx_id: TxId,
    pub lane: AccountId,
    pub seq: Seq,
    pub outcome: AckOutcome,
    pub submitted_at_nanos: u64,
}

crate::layout_claim!(ACK_LAYOUT: Ack, size = 80, crate::layout::LineFit::Straddles(crate::layout::STREAMED));
