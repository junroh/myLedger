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

/// Where a committed batch sits in the log. The one thing recovery needs and nothing here had: a
/// **durable position**, as opposed to the three per-process counters that exist today — the sequencer's
/// committed count, `AccountPort::applied`, and the engine's own — all of which restart at zero.
///
/// It is a batch position rather than an effect position because a batch is what commits: consensus
/// either takes all of it or none, so there is no state a node can be in halfway through one.
///
/// **What still has to happen, and why it is not here.** A snapshot has to record the index its state
/// reflects, and *both* components have to record the same one — otherwise recovery cannot know which of
/// them is further ahead, and the earlier one's replay would re-apply effects the later one already has.
/// The reactor already checks the in-flight version of that invariant every tick, comparing
/// `AccountPort::applied` against its own committed count and sealing on a mismatch
/// (`Broken::AccountViewDisagrees`); what is missing is the check surviving a restart.
///
/// The per-component plumbing — recording this on each side and restoring it — is deliberately not built.
/// The pending engine sits behind a queue, so it cannot be asked synchronously, and what shape the
/// recording takes depends on what the snapshot turns out to be. Design notes §15 has the design and
/// `status.md` has the two decisions it waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ApplyIndex(pub u64);

impl ApplyIndex {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The effect buffer travels back so the sequencer can reuse it instead of allocating.
#[derive(Debug)]
pub struct RaftCommit {
    pub batch_id: u64,
    /// This batch's position in the log. Monotonic and gapless across committed batches, which is what
    /// lets it stand in for "everything up to here has been applied" — see `ApplyIndex`.
    pub index: ApplyIndex,
    pub outcome: RaftOutcome,
    pub effects: Vec<Effect>,
}

/// The log is the only durable truth. Batches commit in proposal order, and the sequencer
/// never waits for one.
pub trait RaftPort {
    fn propose(&self, proposal: RaftProposal) -> Result<(), RaftProposal>;
    fn poll(&self) -> Option<RaftCommit>;
}
