use ledger_base::AccountId;

use crate::config::SafetyPolicy;

/// Lane isolation and fail-stop. A gap is an external component misbehaving: one lane is
/// isolated, several at once mean the component as a whole is broken.
pub struct Safety {
    policy: SafetyPolicy,
    quarantined: Vec<AccountId>,
    fail_stop: bool,
    applies_sealed: bool,
}

impl Safety {
    pub fn new(policy: SafetyPolicy) -> Self {
        Self {
            policy,
            quarantined: Vec::new(),
            fail_stop: false,
            applies_sealed: false,
        }
    }

    pub fn is_fail_stopped(&self) -> bool {
        self.fail_stop
    }

    /// Returns true the first time it trips, so the event is logged once.
    pub fn fail_stop(&mut self) -> bool {
        let first = !self.fail_stop;
        self.fail_stop = true;
        first
    }

    pub fn clear_fail_stop(&mut self) {
        self.fail_stop = false;
    }

    /// A contract-1 violation says an external component is broken; this says *we* are. Consensus
    /// answered for a batch that was not the one waiting, so effects can no longer be paired with
    /// the requests that produced them, and applying any more of them would ack the wrong ones.
    /// There is no operator action for it: the leader has to be replaced.
    pub fn seal_applies(&mut self) -> bool {
        let first = !self.applies_sealed;
        self.applies_sealed = true;
        self.fail_stop = true;
        first
    }

    pub fn applies_sealed(&self) -> bool {
        self.applies_sealed
    }

    /// Returns true if this lane was not already quarantined.
    pub fn quarantine(&mut self, lane: AccountId) -> bool {
        if self.quarantined.contains(&lane) {
            return false;
        }
        self.quarantined.push(lane);
        true
    }

    pub fn release(&mut self, lane: AccountId) {
        self.quarantined.retain(|quarantined| *quarantined != lane);
    }

    pub fn quarantined(&self) -> &[AccountId] {
        &self.quarantined
    }

    pub fn lanes_lost(&self) -> bool {
        self.quarantined.len() >= self.policy.quarantine_fail_stop
    }
}
