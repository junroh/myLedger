use std::collections::VecDeque;

use ledger_base::Transfer;

/// Expiry voids waiting for a slot, kept apart from the client queue because they are a different
/// character of work: no client asked for one, none has a deadline, and the engine offers any that is
/// lost again on its next walk. Rule 10's path separation applied to the one input the ledger sends
/// itself — without it the background path is read first and without a budget, which is where it takes
/// the slots a client's request needed.
///
/// Bounded, and full means **decline**: the notice channel carries the apply-path seal on the same wire,
/// so the reactor has to keep reading it whatever this queue's state is, and a void it cannot park is one
/// it declines rather than one that blocks a seal behind it. Declining costs nothing that is not offered
/// again — a void's id is derived from its hold, so the sweep re-offers exactly the same one.
///
/// How often that happens is counted by the caller, in `Metrics::expiry_dropped`, and not here: one
/// number, one owner.
pub struct ExpiryQueue {
    waiting: VecDeque<Transfer>,
    limit: usize,
}

impl ExpiryQueue {
    pub fn new(limit: usize) -> Self {
        Self {
            waiting: VecDeque::with_capacity(limit),
            limit,
        }
    }

    /// Parks a void for a later tick. False when there was no room.
    pub fn park(&mut self, void: Transfer) -> bool {
        if self.waiting.len() >= self.limit {
            return false;
        }
        self.waiting.push_back(void);
        true
    }

    pub fn take(&mut self) -> Option<Transfer> {
        self.waiting.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_base::{AccountId, TransferFlags, TxId};

    fn void(id: u128) -> Transfer {
        Transfer {
            id: TxId(id),
            pending_ref: TxId(id),
            debit_account: AccountId(1),
            credit_account: AccountId(2),
            amount: 0,
            ledger: 1,
            flags: TransferFlags::VOID_PENDING,
        }
    }

    /// The queue is a bound, not a buffer that grows: past its limit it declines, and declining takes
    /// nothing away from what is already waiting. A queue that grew instead would turn a sweep the
    /// sequencer cannot keep up with into memory nobody bounded.
    #[test]
    fn a_full_queue_declines_without_losing_what_it_holds() {
        let mut queue = ExpiryQueue::new(2);
        assert!(queue.park(void(1)));
        assert!(queue.park(void(2)));
        assert!(!queue.park(void(3)));

        assert_eq!(queue.take().map(|void| void.id), Some(TxId(1)));
        assert_eq!(queue.take().map(|void| void.id), Some(TxId(2)));
        assert!(queue.take().is_none());
    }
}
