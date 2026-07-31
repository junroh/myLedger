use std::hint::black_box;
use std::time::{Duration, Instant};

use ledger_account::MemoryAccounts;
use ledger_base::ports::{AccountFlags, AccountPort};
use ledger_base::{AccountId, AcctHandle, BudgetGroup, Effect, EffectKind, LinkedChainId, TxId};
use ledger_benchkit::{BenchOptions, Samples, STRIDE};

const LEDGER: u32 = 1;
const OPS: u64 = 20_000_000;

/// The in-order apply the account component performs for every committed effect. The effects
/// are walked in order, the way a committed batch is; the accounts they name are scattered, the
/// way real traffic is. Walking the effects randomly instead would measure the effect array's
/// misses rather than the ledger's.
struct ApplyBench {
    accounts: usize,
}

impl ApplyBench {
    fn run(&self) -> Duration {
        let mut store = MemoryAccounts::with_capacity(self.accounts);
        for index in 0..self.accounts {
            store.open(AccountId(index as u64 + 1), LEDGER, AccountFlags::NONE);
        }
        let effects: Vec<Effect> = (0..self.accounts)
            .map(|step| {
                let index = (step as u64).wrapping_mul(STRIDE) as usize % self.accounts;
                let credit = (index + 1) % self.accounts;
                Effect {
                    tx_id: TxId(index as u128 + 1),
                    pending_ref: TxId::ABSENT,
                    debit_account: AccountId(index as u64 + 1),
                    credit_account: AccountId(credit as u64 + 1),
                    amount: 1,
                    remaining_after: 0,
                    debit: AcctHandle::new(index),
                    credit: AcctHandle::new(credit),
                    chain: LinkedChainId::ABSENT,
                    budget: BudgetGroup::ABSENT,
                    ledger: LEDGER,
                    kind: EffectKind::Post,
                }
            })
            .collect();

        let started = Instant::now();
        for step in 0..OPS {
            let _ = black_box(store.apply(&effects[step as usize % effects.len()]));
        }
        started.elapsed()
    }
}

/// Does the record's size matter, given it is reached at random? A 40-byte record packs
/// tighter but straddles cache lines; a 64-byte one never does, on either line size, because 64
/// divides both. Same field updates, same access pattern, so only the layout differs.
struct StrideBench {
    accounts: usize,
    padded: bool,
}

/// The fields are the point: they are the bytes being measured.
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct Packed {
    debits_posted: i64,
    credits_posted: i64,
    debits_pending: i64,
    credits_pending: i64,
    ledger: u32,
    flags: u8,
}

#[derive(Clone, Copy, Default)]
#[repr(align(64))]
#[allow(dead_code)]
struct Padded {
    debits_posted: i64,
    credits_posted: i64,
    debits_pending: i64,
    credits_pending: i64,
    ledger: u32,
    flags: u8,
}

impl StrideBench {
    fn run(&self) -> Duration {
        let mut packed = vec![Packed::default(); if self.padded { 0 } else { self.accounts }];
        let mut padded = vec![Padded::default(); if self.padded { self.accounts } else { 0 }];
        let started = Instant::now();
        for step in 0..OPS {
            let index = (step.wrapping_mul(STRIDE) % self.accounts as u64) as usize;
            if self.padded {
                let entry = &mut padded[index];
                entry.debits_posted += 1;
                entry.credits_pending -= 1;
                black_box(entry.ledger + u32::from(entry.flags));
            } else {
                let entry = &mut packed[index];
                entry.debits_posted += 1;
                entry.credits_pending -= 1;
                black_box(entry.ledger + u32::from(entry.flags));
            }
        }
        started.elapsed()
    }
}

fn main() {
    let options = BenchOptions::from_args();
    options.announce();
    for accounts in [1_000usize, 1_000_000, 8_000_000] {
        for padded in [false, true] {
            let bench = StrideBench { accounts, padded };
            let layout = if padded { "64B padded" } else { "40B packed" };
            let mut samples = Samples::new(format!("record {layout} ({accounts} acct)"), OPS);
            for _ in 0..options.repeat {
                samples.add(bench.run());
            }
            samples.report();
        }
    }

    for accounts in [1_000usize, 100_000, 1_000_000, 8_000_000] {
        let bench = ApplyBench { accounts };
        let mut samples = Samples::new(format!("apply ({accounts} acct)"), OPS);
        for _ in 0..options.repeat {
            samples.add(bench.run());
        }
        samples.report();
    }
}
