use std::collections::VecDeque;

use ledger_base::{
    AccountId, Ack, AckOutcome, Amount, FxHashMap, Prng, Transfer, TransferFlags, TxId,
};

pub const EXTERNAL_ACCOUNT: AccountId = AccountId(1);
pub const CLEARING_ACCOUNT: AccountId = AccountId(2);
const FIRST_USER_ACCOUNT: u64 = 1000;
const LEDGER: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    SinglePhase,
    HoldSettle,
    PartialSettle,
    VoidHeavy,
    HotLane,
    Linked,
}

impl WorkloadKind {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "single-phase" => Some(Self::SinglePhase),
            "hold-settle" => Some(Self::HoldSettle),
            "partial-settle" => Some(Self::PartialSettle),
            "void-heavy" => Some(Self::VoidHeavy),
            "hot-lane" => Some(Self::HotLane),
            "linked" => Some(Self::Linked),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::SinglePhase => "single-phase",
            Self::HoldSettle => "hold-settle",
            Self::PartialSettle => "partial-settle",
            Self::VoidHeavy => "void-heavy",
            Self::HotLane => "hot-lane",
            Self::Linked => "linked",
        }
    }

    pub const fn all() -> [Self; 6] {
        [
            Self::SinglePhase,
            Self::HoldSettle,
            Self::PartialSettle,
            Self::VoidHeavy,
            Self::HotLane,
            Self::Linked,
        ]
    }
}

struct OpenHold {
    id: TxId,
    debit: AccountId,
    credit: AccountId,
    remaining: Amount,
}

/// How the request stream is spread over accounts.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    pub accounts: u64,
    /// 1 is uniform; higher picks low-numbered accounts far more often.
    pub skew: f64,
    /// Fraction of transfers debiting the unconstrained clearing account.
    pub external_ratio: f64,
    /// How old a hold is when it is resolved, counted in holds created since. Zero resolves each one as
    /// soon as its commit is acked, which is what every workload did before this existed — and why a
    /// record was never asked for after it had been written.
    pub resolve_after: usize,
}

/// Generates the request stream and tracks which holds exist, so settles and voids
/// reference holds the ledger has actually committed.
pub struct Workload {
    kind: WorkloadKind,
    accounts: u64,
    skew: f64,
    external_ratio: f64,
    transfer_amount: Amount,
    funding_amount: Amount,
    rng: Prng,
    next_id: u128,
    submitted_holds: FxHashMap<TxId, OpenHold>,
    /// Committed holds waiting to be resolved, oldest first. A queue rather than a stack because the
    /// oldest is the one whose record may have moved out of memory, which is the case worth reaching.
    ready_holds: VecDeque<OpenHold>,
    resolve_after: usize,
    /// Legs of the chain being submitted, after the one already handed out. The batch must not end
    /// while any are left.
    queued_chain_legs: VecDeque<Transfer>,
}

impl Workload {
    pub fn new(kind: WorkloadKind, shape: Shape, seed: u64) -> Self {
        Self {
            kind,
            accounts: shape.accounts.max(2),
            skew: shape.skew.max(1.0),
            external_ratio: shape.external_ratio.clamp(0.0, 1.0),
            resolve_after: shape.resolve_after,
            transfer_amount: 1_000,
            funding_amount: 1_000_000_000,
            rng: Prng::new(seed),
            next_id: 1,
            submitted_holds: FxHashMap::default(),
            ready_holds: VecDeque::new(),
            queued_chain_legs: VecDeque::new(),
        }
    }

    pub fn user_account(&self, index: u64) -> AccountId {
        AccountId(FIRST_USER_ACCOUNT + index % self.accounts)
    }

    pub fn account_count(&self) -> u64 {
        self.accounts
    }

    pub fn funding_transfer(&mut self, index: u64) -> Transfer {
        Transfer {
            id: self.take_id(),
            pending_ref: TxId::ABSENT,
            debit_account: EXTERNAL_ACCOUNT,
            credit_account: self.user_account(index),
            amount: self.funding_amount,
            ledger: LEDGER,
            flags: TransferFlags::NONE,
        }
    }

    pub fn next(&mut self) -> Transfer {
        match self.kind {
            WorkloadKind::SinglePhase => self.post(),
            WorkloadKind::HotLane => self.hot_post(),
            WorkloadKind::HoldSettle => match self.due_hold() {
                Some(hold) => self.settle(hold, false),
                None => self.hold(),
            },
            WorkloadKind::PartialSettle => match self.due_hold() {
                Some(hold) => self.settle(hold, true),
                None => self.hold(),
            },
            WorkloadKind::VoidHeavy => match self.due_hold() {
                Some(hold) => self.void(hold),
                None => self.hold(),
            },
            WorkloadKind::Linked => self.chain_leg(),
        }
    }

    /// The oldest hold, once enough have been created behind it. Until then the workload keeps creating,
    /// which is what gives a hold an age at all: with no queue to wait in, every record is read back
    /// moments after it was written and no window past the first is ever tested.
    fn due_hold(&mut self) -> Option<OpenHold> {
        if self.ready_holds.len() <= self.resolve_after {
            return None;
        }
        self.ready_holds.pop_front()
    }

    /// True while a linked chain is half-submitted: the batch must not end here.
    pub fn chain_open(&self) -> bool {
        !self.queued_chain_legs.is_empty()
    }

    /// A hold only becomes referenceable once its commit is acked; settling earlier would
    /// be a client-side race, not a ledger property under test.
    pub fn on_ack(&mut self, ack: &Ack) {
        let Some(hold) = self.submitted_holds.remove(&ack.tx_id) else {
            return;
        };
        if ack.outcome == AckOutcome::Committed {
            self.ready_holds.push_back(hold);
        }
    }

    /// One leg of a chain, starting a new chain when none is open. Two shapes, alternating: money
    /// arrives and the second leg spends it, which only the chain's own scratch makes possible; or a
    /// hold is created and resolved inside the chain that created it — the one resolution judged with
    /// no record, so the engine reads back the version it appended (the in-chain fallback).
    fn chain_leg(&mut self) -> Transfer {
        if let Some(leg) = self.queued_chain_legs.pop_front() {
            return leg;
        }
        if self.rng.next_u64().is_multiple_of(2) {
            self.fund_then_spend_chain()
        } else {
            self.hold_then_resolve_chain()
        }
    }

    fn fund_then_spend_chain(&mut self) -> Transfer {
        let user = self.random_user();
        let credit = self.other_user(user);
        let spend = Transfer {
            id: self.take_id(),
            pending_ref: TxId::ABSENT,
            debit_account: user,
            credit_account: credit,
            amount: self.transfer_amount,
            ledger: LEDGER,
            flags: TransferFlags::NONE,
        };
        self.queued_chain_legs.push_back(spend);
        Transfer {
            id: self.take_id(),
            pending_ref: TxId::ABSENT,
            debit_account: EXTERNAL_ACCOUNT,
            credit_account: user,
            amount: self.transfer_amount,
            ledger: LEDGER,
            flags: TransferFlags::LINKED,
        }
    }

    /// Hold, partial settle, void of the rest — all in one chain. The settle is judged against the
    /// chain's scratch rather than a record, so its committed `Reduce` carries no original size and
    /// the engine falls back to reading the version it appended moments ago. No workload reached that
    /// path before; its cost was covered by a unit test and nothing else. The hold declares no budget
    /// group (`pending_ref` absent), because a group member may only move whole.
    fn hold_then_resolve_chain(&mut self) -> Transfer {
        let user = self.random_user();
        let hold_id = self.take_id();
        let settle = Transfer {
            id: self.take_id(),
            pending_ref: hold_id,
            debit_account: EXTERNAL_ACCOUNT,
            credit_account: user,
            amount: (self.transfer_amount / 2).max(1),
            ledger: LEDGER,
            flags: TransferFlags::POST_PENDING | TransferFlags::LINKED,
        };
        let void_rest = Transfer {
            id: self.take_id(),
            pending_ref: hold_id,
            debit_account: EXTERNAL_ACCOUNT,
            credit_account: user,
            amount: 0,
            ledger: LEDGER,
            flags: TransferFlags::VOID_PENDING,
        };
        self.queued_chain_legs.push_back(settle);
        self.queued_chain_legs.push_back(void_rest);
        Transfer {
            id: hold_id,
            pending_ref: TxId::ABSENT,
            debit_account: EXTERNAL_ACCOUNT,
            credit_account: user,
            amount: self.transfer_amount,
            ledger: LEDGER,
            flags: TransferFlags::PENDING | TransferFlags::LINKED,
        }
    }

    fn post(&mut self) -> Transfer {
        let debit = self.debit_account();
        let credit = self.other_user(debit);
        Transfer {
            id: self.take_id(),
            pending_ref: TxId::ABSENT,
            debit_account: debit,
            credit_account: credit,
            amount: self.transfer_amount,
            ledger: LEDGER,
            flags: TransferFlags::NONE,
        }
    }

    fn hot_post(&mut self) -> Transfer {
        let credit = self.random_user();
        Transfer {
            id: self.take_id(),
            pending_ref: TxId::ABSENT,
            debit_account: CLEARING_ACCOUNT,
            credit_account: credit,
            amount: self.transfer_amount,
            ledger: LEDGER,
            flags: TransferFlags::NONE,
        }
    }

    fn hold(&mut self) -> Transfer {
        let debit = self.debit_account();
        let credit = self.other_user(debit);
        let id = self.take_id();
        self.submitted_holds.insert(
            id,
            OpenHold {
                id,
                debit,
                credit,
                remaining: self.transfer_amount,
            },
        );
        Transfer {
            id,
            pending_ref: TxId::ABSENT,
            debit_account: debit,
            credit_account: credit,
            amount: self.transfer_amount,
            ledger: LEDGER,
            flags: TransferFlags::PENDING,
        }
    }

    fn settle(&mut self, mut hold: OpenHold, partial: bool) -> Transfer {
        let amount = if partial {
            (hold.remaining / 2).max(1)
        } else {
            hold.remaining
        };
        hold.remaining -= amount;
        let transfer = Transfer {
            id: self.take_id(),
            pending_ref: hold.id,
            debit_account: hold.debit,
            credit_account: hold.credit,
            amount,
            ledger: LEDGER,
            flags: TransferFlags::POST_PENDING,
        };
        if hold.remaining > 0 {
            // Back to the end of the queue, so a partly settled hold ages again before its next
            // resolution — which is the case that reads a record the previous settle moved.
            self.ready_holds.push_back(hold);
        }
        transfer
    }

    fn void(&mut self, hold: OpenHold) -> Transfer {
        Transfer {
            id: self.take_id(),
            pending_ref: hold.id,
            debit_account: hold.debit,
            credit_account: hold.credit,
            amount: 0,
            ledger: LEDGER,
            flags: TransferFlags::VOID_PENDING,
        }
    }

    /// The clearing account needs no balance check, but it is still one lane: traffic sent here
    /// concentrates ordering on a single lane rather than escaping it.
    fn debit_account(&mut self) -> AccountId {
        if self.external_ratio > 0.0 && self.unit() < self.external_ratio {
            return CLEARING_ACCOUNT;
        }
        self.random_user()
    }

    fn random_user(&mut self) -> AccountId {
        AccountId(FIRST_USER_ACCOUNT + self.account_index())
    }

    /// Uniform at skew 1. Above it, `u^skew` bunches the draw toward the low indices, which is the
    /// hot-account shape: a few accounts take most of the traffic.
    fn account_index(&mut self) -> u64 {
        if self.skew <= 1.0 {
            return self.rng.next_u64() % self.accounts;
        }
        let index = (self.accounts as f64 * self.unit().powf(self.skew)) as u64;
        index.min(self.accounts - 1)
    }

    fn unit(&mut self) -> f64 {
        (self.rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn other_user(&mut self, avoid: AccountId) -> AccountId {
        let candidate = self.random_user();
        if candidate == avoid {
            return AccountId(
                FIRST_USER_ACCOUNT + (candidate.raw() + 1 - FIRST_USER_ACCOUNT) % self.accounts,
            );
        }
        candidate
    }

    fn take_id(&mut self) -> TxId {
        let id = TxId(self.next_id);
        self.next_id += 1;
        id
    }
}
