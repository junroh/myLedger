use crate::ids::{AccountId, Seq, TxId};
use crate::ports::Correlation;

/// Dedup fact from the idempotency engine. The verdict is data; the response to the client
/// is the sequencer's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdemVerdict {
    Fresh,
    DuplicateSameBody,
    DuplicateDifferentBody,
}

#[derive(Debug, Clone, Copy)]
pub struct IdemRequest {
    pub correlation: Correlation,
    pub tx_id: TxId,
    pub lane: AccountId,
    pub seq: Seq,
    pub digest: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct IdemReply {
    pub correlation: Correlation,
    pub lane: AccountId,
    pub seq: Seq,
    pub verdict: IdemVerdict,
}

/// Each verdict is independent of every other, which is what makes this the one component
/// that can be moved off the reactor core entirely. Replies still come back in lane order.
pub trait IdempotencyPort {
    fn dispatch(&self, request: IdemRequest) -> Result<(), IdemRequest>;
    fn poll(&self) -> Option<IdemReply>;
}
