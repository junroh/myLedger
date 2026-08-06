use crate::ids::{AccountId, Seq, TxId};
use crate::ports::Correlation;

/// Idem fact from the idempotency engine. The verdict is data; the response to the client
/// is the sequencer's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdemVerdict {
    Fresh,
    DuplicateSameBody,
    DuplicateDifferentBody,
    /// Nothing was asked about this id. The request wanted its place in the lane's order and no
    /// judgment, so this is the absence of a claim rather than a claim that the id is new.
    NotChecked,
}

/// What this component is being asked for. It does two things that are easy to read as one.
///
/// **Recording** an id is what makes a client's retry safe: the second arrival of a transaction is
/// answered instead of applied again. **Ordering** is not a property of the record at all — it comes
/// from the queue the answer travels back through, which delivers a lane in seq order (contract 1). A
/// request can need the second without the first, and one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdemAsk {
    /// Record the id, and say whether it was already there.
    Check,
    /// Take a place in the lane's order and nothing else: record nothing, judge nothing.
    ///
    /// This is for a transaction the ledger derives rather than receives. Its id comes from the hold it
    /// resolves, so it cannot be given a fresh one — and recording an id at dispatch, before consensus
    /// has accepted it, would leave a refused resolution permanently answered as a duplicate and its
    /// hold permanently unreleasable. It needs no record for a different reason: the index that produced
    /// it is what makes it unrepeatable. What it does still need is to be judged in its lane's turn.
    Serialize,
}

#[derive(Debug, Clone, Copy)]
pub struct IdemRequest {
    pub correlation: Correlation,
    pub tx_id: TxId,
    pub lane: AccountId,
    pub seq: Seq,
    pub digest: u64,
    pub ask: IdemAsk,
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
