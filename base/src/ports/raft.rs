use crate::effect::Effect;

#[derive(Debug)]
pub struct RaftProposal {
    pub batch_id: u64,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftOutcome {
    Committed,
    Failed,
}

/// The effect buffer travels back so the sequencer can reuse it instead of allocating.
#[derive(Debug)]
pub struct RaftCommit {
    pub batch_id: u64,
    pub outcome: RaftOutcome,
    pub effects: Vec<Effect>,
}

/// The log is the only durable truth. Batches commit in proposal order, and the sequencer
/// never waits for one.
pub trait RaftPort {
    fn propose(&self, proposal: RaftProposal) -> Result<(), RaftProposal>;
    fn poll(&self) -> Option<RaftCommit>;
}
