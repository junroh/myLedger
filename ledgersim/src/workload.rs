//! What a client sends: every shape the ledger accepts, and the shapes it refuses. Driven by the
//! seed, so the same seed sends the same stream.

use std::collections::VecDeque;

use ledger_base::{AccountId, Ack, Amount, FxHashMap, Prng, Request, Transfer, TransferFlags, TxId};

pub const LEDGER: u32 = 1;
pub const EXTERNAL: AccountId = AccountId(1);
pub const FIRST_USER: u64 = 100;
pub const FUNDING: Amount = 1_000_000;

/// A hold the ledger has told us about, so resolutions can aim at one that exists — and, some of the
/// time, at one it has already finished.
#[derive(Clone, Copy)]
struct Hold {
    id: TxId,
    debit: AccountId,
    credit: AccountId,
    amount: Amount,
    committed: bool,
}

/// The holds a client still remembers: a ring where the newest overwrites the oldest, and an index so
/// an ack finds its own hold instead of walking the ring. It has to outlast a round trip's worth of
/// submissions, or an ack arrives to find its hold already forgotten, nothing is ever known to have
/// committed, and the client stops resolving anything at all.
struct Holds {
    ring: Vec<Hold>,
    /// Which slot the next hold overwrites, once the ring is full.
    oldest: usize,
    slot: FxHashMap<TxId, usize>,
    capacity: usize,
}

impl Holds {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(64);
        Self { ring: Vec::with_capacity(capacity), oldest: 0, slot: FxHashMap::default(), capacity }
    }

    fn remember(&mut self, hold: Hold) {
        if self.ring.len() < self.capacity {
            self.slot.insert(hold.id, self.ring.len());
            self.ring.push(hold);
            return;
        }
        self.slot.remove(&self.ring[self.oldest].id);
        self.slot.insert(hold.id, self.oldest);
        self.ring[self.oldest] = hold;
        self.oldest = (self.oldest + 1) % self.capacity;
    }

    fn find(&mut self, id: TxId) -> Option<&mut Hold> {
        let slot = *self.slot.get(&id)?;
        self.ring.get_mut(slot)
    }

    fn len(&self) -> usize {
        self.ring.len()
    }

    fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    fn at(&self, index: usize) -> Hold {
        self.ring[index]
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

pub struct Traffic {
    prng: Prng,
    next_id: u128,
    holds: Holds,
    last: Option<Transfer>,
    burst: u64,
    accounts: u64,
    /// A client that only resolves holds it has been told committed. Capacity asks what the ledger
    /// does for traffic like that; refusals are `check`'s question, and a run spending most of its
    /// core on them would predict nothing useful.
    strict: bool,
    /// Holds the ledger said committed, in commit order: what a client would resolve next. Bounded
    /// like the ring, because a client falling further and further behind is offering load rather
    /// than remembering it.
    ready: VecDeque<Hold>,
    /// Above 1, `u^skew` bunches accounts toward the low indices: the hot-account shape. Same formula
    /// as the load driver's `--skew`, so a number means the same thing in both.
    skew: f64,
}

impl Traffic {
    pub fn new(
        seed: u64,
        burst: u64,
        accounts: u64,
        strict: bool,
        remembered: usize,
        skew: f64,
    ) -> Self {
        Self {
            prng: Prng::new(seed),
            next_id: 1,
            holds: Holds::new(remembered),
            last: None,
            burst,
            accounts,
            strict,
            ready: VecDeque::new(),
            skew: skew.max(1.0),
        }
    }

    pub fn accounts(&self) -> u64 {
        self.accounts
    }

    pub fn funding(&mut self, account: AccountId, amount: Amount) -> Transfer {
        self.transfer(EXTERNAL, account, amount)
    }

    /// A hold becomes resolvable once the ledger says it committed; a resolution of one that was refused
    /// is a shape the ledger has to reject, so it stays in the ring to be aimed at.
    pub fn answered(&mut self, ack: &Ack, committed: bool) {
        let Some(hold) = self.holds.find(ack.tx_id) else {
            return;
        };
        hold.committed = committed;
        if !committed {
            return;
        }
        let hold = *hold;
        self.ready.push_back(hold);
        if self.ready.len() > self.holds.capacity() {
            self.ready.pop_front();
        }
    }

    pub fn next(&mut self, now: u64) -> Vec<Request> {
        let bursts = self.prng.next_u64() % self.burst.max(1);
        let mut requests = Vec::new();
        for _ in 0..bursts {
            requests.extend(self.single(now));
        }
        requests
    }

    /// One thing a client does, which is one request unless it is a chain.
    pub fn single(&mut self, now: u64) -> Vec<Request> {
        match self.prng.next_u64() % 8 {
            0 | 1 => {
                let transfer = self.post();
                self.last = Some(transfer);
                vec![Request::single(transfer, now)]
            }
            // The same transaction again: a duplicate, not a second transfer.
            2 => self
                .last
                .map(|tx| Request::single(tx, now))
                .into_iter()
                .collect(),
            3 | 4 => vec![Request::single(self.hold(), now)],
            5 => self.resolution(TransferFlags::POST_PENDING, now),
            6 => self.resolution(TransferFlags::VOID_PENDING, now),
            _ => self.chain(now),
        }
    }

    fn post(&mut self) -> Transfer {
        let debit = self.user();
        let credit = self.other(debit);
        let amount = self.amount();
        self.transfer(debit, credit, amount)
    }

    fn hold(&mut self) -> Transfer {
        let debit = self.user();
        let credit = self.other(debit);
        let amount = self.amount();
        let mut transfer = self.transfer(debit, credit, amount);
        transfer.flags = TransferFlags::PENDING;
        self.holds.remember(Hold { id: transfer.id, debit, credit, amount, committed: false });
        transfer
    }

    /// Settles are sometimes partial and sometimes more than the hold has left; voids take whatever
    /// remains. Both are aimed at holds that may never have committed.
    fn resolution(&mut self, flags: TransferFlags, now: u64) -> Vec<Request> {
        if self.holds.is_empty() {
            return Vec::new();
        }
        let hold = if self.strict {
            // A client resolves a hold it was told committed, once. The queue is in commit order.
            match self.ready.pop_front() {
                Some(hold) => hold,
                // Nothing to resolve yet: a client would send something else.
                None => return Vec::new(),
            }
        } else {
            // Mostly aim at a hold the ledger said it committed, which is what a client does; the rest
            // of the time aim anywhere, so the refusals are exercised too. A few tries rather than a
            // scan: this runs for every resolution the simulation offers.
            let mut index = (self.prng.next_u64() % self.holds.len() as u64) as usize;
            if !self.prng.next_u64().is_multiple_of(5) {
                for _ in 0..8 {
                    if self.holds.at(index).committed {
                        break;
                    }
                    index = (self.prng.next_u64() % self.holds.len() as u64) as usize;
                }
            }
            self.holds.at(index)
        };
        let amount = if flags == TransferFlags::VOID_PENDING {
            0
        } else {
            1 + (self.prng.next_u64() % (hold.amount as u64 + 200)) as Amount
        };
        let mut transfer = self.transfer(hold.debit, hold.credit, amount);
        transfer.pending_ref = hold.id;
        transfer.flags = flags;
        vec![Request::single(transfer, now)]
    }

    /// Two legs in one submission: money into an account, then out of it again, which only the
    /// chain's own scratch makes possible.
    fn chain(&mut self, now: u64) -> Vec<Request> {
        let user = self.user();
        let other = self.other(user);
        let amount = self.amount();
        let mut first = self.transfer(EXTERNAL, user, amount);
        first.flags = TransferFlags::LINKED;
        let second = self.transfer(user, other, amount);
        vec![
            Request {
                tx: first,
                submitted_at_nanos: now,
                end_of_batch: false,
            },
            Request {
                tx: second,
                submitted_at_nanos: now,
                end_of_batch: true,
            },
        ]
    }

    fn transfer(&mut self, debit: AccountId, credit: AccountId, amount: Amount) -> Transfer {
        let id = TxId(self.next_id);
        self.next_id += 1;
        Transfer {
            id,
            pending_ref: TxId::ABSENT,
            debit_account: debit,
            credit_account: credit,
            amount,
            ledger: LEDGER,
            flags: TransferFlags::NONE,
        }
    }

    fn amount(&mut self) -> Amount {
        1 + (self.prng.next_u64() % 500) as Amount
    }

    fn user(&mut self) -> AccountId {
        AccountId(FIRST_USER + self.account_index())
    }

    fn account_index(&mut self) -> u64 {
        if self.skew <= 1.0 {
            return self.prng.next_u64() % self.accounts;
        }
        let index = (self.accounts as f64 * self.prng.next_float().powf(self.skew)) as u64;
        index.min(self.accounts - 1)
    }

    fn other(&mut self, avoid: AccountId) -> AccountId {
        let candidate = self.user();
        if candidate == avoid {
            AccountId(FIRST_USER + (candidate.raw() + 1 - FIRST_USER) % self.accounts)
        } else {
            candidate
        }
    }
}
